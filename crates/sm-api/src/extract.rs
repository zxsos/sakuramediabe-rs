//! 请求体提取器包装。
//!
//! # 为什么不能直接 `axum::Json`
//!
//! axum 里提取器失败时，**rejection 直接变成响应**，不会经过 handler 的
//! 错误类型。所以 `Json<PlaylistCreateRequest>` 解析失败时，客户端拿到的是
//! axum 默认的 **400 + 纯文本**，而不是上游的
//! `422 {"error":{"code":"validation_error",...}}`。
//!
//! 这是个静默的契约破坏：状态码和响应体形状都错了，而单测如果只断言
//! "请求失败了"根本发现不了。
//!
//! 所以这里包一层，把 `JsonRejection` 转成 [`ErrorResponse`]。
//!
//! [`Multipart`] 是同一个理由的第二个例子：axum 的 `Multipart` 解析失败同样
//! 直接变成响应（400 + 纯文本），而上游的 `RequestValidationError` 是
//! 422 信封。

use std::path::Path;

use axum::extract::multipart::MultipartRejection;
use axum::extract::{
    Form as AxumForm, FromRequest, Json as AxumJson, Multipart as AxumMultipart,
    Query as AxumQuery, Request,
};
use axum::http::StatusCode;
// 重复 query 参数的解析器（`serde_html_form`）—— 只 [`HtmlFormQuery`] 用。
use axum_extra::extract::Query as AxumExtraQuery;
use serde::de::DeserializeOwned;
// 流式落盘（`receive_to_file`）：100 MiB 的插件包不能全缓冲进内存。
use tokio::io::AsyncWriteExt;

use crate::error::ErrorResponse;

/// 请求体提取器：解析失败时产出上游形状的 422。
pub struct Json<T>(pub T);

impl<T, S> FromRequest<S> for Json<T>
where
    T: DeserializeOwned + Send + 'static,
    S: Send + Sync,
{
    type Rejection = ErrorResponse;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match AxumJson::<T>::from_request(request, state).await {
            Ok(AxumJson(value)) => Ok(Self(value)),
            Err(rejection) => Err(ErrorResponse::from(rejection)),
        }
    }
}

/// 查询参数提取器：解析失败时产出上游形状的 422。
///
/// # 为什么 `Query` 也要包 —— 它和 `Json` 是同一个漏洞
///
/// axum 的 `QueryRejection` 默认响应是 **400 + 纯文本**
/// （`Failed to deserialize query string: ...`），既不经过错误信封，
/// 状态码也与上游 FastAPI 的 422 不符。
///
/// 这个漏洞是**加查询参数时才暴露**的：本文件写下时项目里一个查询参数都
/// 没有，所以只包了 `Json` 与 `Multipart`。第一个查询参数
/// （`?include_system=`）立刻就撞上了 —— 由
/// `crates/sm-api/tests/playlists_http.rs` 的
/// `a_bad_query_value_returns_the_error_envelope` 钉住。
///
/// 用法与 `Json` 相同：写 `EnvelopeQuery(q): EnvelopeQuery<Q>`。
pub struct Query<T>(pub T);

impl<T, S> FromRequest<S> for Query<T>
where
    T: DeserializeOwned + Send + 'static,
    S: Send + Sync,
{
    type Rejection = ErrorResponse;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match AxumQuery::<T>::from_request(request, state).await {
            Ok(AxumQuery(value)) => Ok(Self(value)),
            Err(rejection) => Err(ErrorResponse::from(rejection)),
        }
    }
}

/// 查询参数提取器（**唯一支持重复键的那个**）。
///
/// # 为什么不能复用 [`Query`]
///
/// axum 自带的 `Query` 走 `serde_urlencoded`，而后者把「字段期待序列」直接
/// 转发成 `visit_str`（`serde_urlencoded-0.7.1/src/de.rs` 的 `Part`：`seq`
/// 落在 `forward_to_deserialize_any!` 里）。于是：
///
/// ```text
/// ?state=a&state=b   -> invalid type: string "a", expected a sequence
/// ?state=a           -> 同样失败
/// ```
///
/// 也就是说「重复键」与「单个值」**都会** 422，而不是「取最后一个」。
///
/// 上游 `GET /download-tasks` 的 `state` 是 `list[str] = Query(default=None)`
/// （重复键），客户端（Flutter dio 的 `ListFormat.multi`）就是这么发的 ——
/// 那条路由必须走 html-form 解析（`axum_extra::extract::Query`，内部是
/// `serde_html_form`）。
///
/// # 只在那条路由上用
///
/// `serde_html_form` 与 `serde_urlencoded` 对**平面标量**的解析基本一致，
/// 但把全仓的查询解析器换掉是无谓的爆炸半径（每条已接端点的 HTTP 用例都在
/// 它的影响面里）。所以这里**新增**一个提取器，而不是改 [`Query`]。
///
/// 用法与 [`Query`] 相同：`HtmlFormQuery(q): HtmlFormQuery<Q>`。
pub struct HtmlFormQuery<T>(pub T);

impl<T, S> FromRequest<S> for HtmlFormQuery<T>
where
    T: DeserializeOwned + Send + 'static,
    S: Send + Sync,
{
    type Rejection = ErrorResponse;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match AxumExtraQuery::<T>::from_request(request, state).await {
            Ok(AxumExtraQuery(value)) => Ok(Self(value)),
            Err(rejection) => Err(ErrorResponse::from(rejection)),
        }
    }
}

/// 表单提取器（`application/x-www-form-urlencoded`）：解析失败时产出上游形状的 422。
///
/// # 为什么也要包
///
/// 与 [`Json`] 同一个漏洞：axum 的 `FormRejection` **不经过错误信封**，而且
/// 「content-type 不是 form」回 415、「字段缺失」回 422，都是纯文本。上游
/// `POST /auth/docs-token` 收的是 `OAuth2PasswordRequestForm`（表单），校验失败
/// 走的是同一个 `RequestValidationError`（422 + 信封）。
///
/// 用法与 [`Json`] 相同：`EnvelopeForm(f): EnvelopeForm<F>`。
pub struct Form<T>(pub T);

impl<T, S> FromRequest<S> for Form<T>
where
    T: DeserializeOwned + Send + 'static,
    S: Send + Sync,
{
    type Rejection = ErrorResponse;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match AxumForm::<T>::from_request(request, state).await {
            Ok(AxumForm(value)) => Ok(Self(value)),
            Err(rejection) => Err(ErrorResponse::from(rejection)),
        }
    }
}

/// 单个上传文件。
#[derive(Debug, Clone)]
pub struct UploadedFile {
    /// 表单字段名。
    pub field: String,
    /// 客户端给的文件名。可能为 `None`（未带 `filename=` 的字段）。
    pub file_name: Option<String>,
    /// 声明的 MIME 类型。**不校验** —— 上游也不校验，而这里校验会把
    /// 「类型不认识的合法文件」变成 422。
    pub content_type: Option<String>,
    /// 文件内容。
    pub bytes: Vec<u8>,
}

/// multipart 提取器，带**强制字节上限**。
///
/// # 上限为什么必须是强制的
///
/// 不设上限的话，一个 `Content-Length: 1 GB` 的上传会在内存里堆到 1 GB ——
/// 鉴权中间件挡不住（body 在鉴权之后才读），axum 的默认 body 上限是 2 MB
/// 但那也足够把一个 16 GB 的 NAS 机器的内存吃干。这里默认
/// [`DEFAULT_MAX_BYTES`]，由调用方按端点调大或调小。
///
/// 超限 → **413** `http_error`（`Payload Too Large`）。不是 422：422 在
/// 这个项目里表示「请求体格式/字段不对」，而文件太大是另一回事，客户端
/// 据此该提示「换个文件」而不是「改改字段」。
///
/// # 为什么流式累积而不是 `bytes()` 一次性读
///
/// `Field::bytes()` 会把整个字段读进内存且**没有上限**。这里逐 chunk
/// 累加并在超限时**立刻**返回：超限那一刻已经读进来的字节被丢弃，
/// 不会继续读到请求结束。
pub struct Multipart {
    inner: AxumMultipart,
    max_bytes: usize,
}

impl Multipart {
    /// 单个文件的上限。默认 [`DEFAULT_MAX_BYTES`]。
    pub fn new(inner: AxumMultipart) -> Self {
        Self {
            inner,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// 覆盖上限。
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// 取下一个文件字段。流已结束 → `Ok(None)`。
    ///
    /// 解析失败 → 422 `validation_error`（与 JSON body 同类）；
    /// 超限 → 413 `http_error`。
    pub async fn next_file(&mut self) -> Result<Option<UploadedFile>, ErrorResponse> {
        loop {
            let mut field = match self.inner.next_field().await.map_err(ErrorResponse::from)? {
                Some(field) => field,
                None => return Ok(None),
            };

            let mut file = UploadedFile {
                field: field.name().unwrap_or_default().to_owned(),
                file_name: field.file_name().map(str::to_owned),
                content_type: field.content_type().map(str::to_owned),
                bytes: Vec::new(),
            };

            while let Some(chunk) = field.chunk().await.map_err(ErrorResponse::from)? {
                // `len + chunk.len()` 而不是「先加再加判」：后者会先分配
                // 出超限的整块内存再丢掉它，前者只多占一个 chunk 的量。
                if file.bytes.len() + chunk.len() > self.max_bytes {
                    return Err(ErrorResponse::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "http_error",
                        "Uploaded file is too large",
                    )
                    .with_details(details_of_pair(
                        "field",
                        &file.field,
                        "max_bytes",
                        self.max_bytes as i64,
                    )));
                }
                file.bytes.extend_from_slice(&chunk);
            }

            // 跳过非文件字段（表单里的普通文本输入）。
            if file.file_name.is_none() && file.bytes.is_empty() {
                continue;
            }
            return Ok(Some(file));
        }
    }
}

/// 默认单文件上限 8 MiB。
///
/// 取 8 MiB 的理由与 `media-file-hash` 的采样阈值同源：视频封面/插件 zip
/// 都在这个量级以内，而超限的实际上传是**误选了一个视频文件**。
pub const DEFAULT_MAX_BYTES: usize = 8 * 1024 * 1024;

fn details_of_pair(
    key_a: &str,
    value_a: &str,
    key_b: &str,
    value_b: i64,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert(key_a.to_owned(), serde_json::Value::from(value_a));
    map.insert(key_b.to_owned(), serde_json::Value::from(value_b));
    map
}

/// 单个**非文件**字段的长度上限（64 KiB）。
///
/// 上游没有这一条（FastAPI 把文本字段读进内存，不设限）。这里加上是因为
/// 本模块的流式路径整体要靠 [`receive_to_file`] 的 `max_bytes` 兜底，
/// 而「一个 500 MB 的普通表单字段」在流式路径里没有别的闸门。64 KiB 足够
/// 装下任何真实字段（`sha256` / `enable` 都只有几个字节）。
pub const MAX_TEXT_FIELD_BYTES: usize = 64 * 1024;

/// [`receive_to_file`] 收到的多部分表单。
#[derive(Debug, Clone, Default)]
pub struct ReceivedForm {
    /// **第一个**文件字段。`None` = 请求里一个文件字段都没有。
    ///
    /// 出现第二个及以后的文件字段时它们被**丢弃**（仍计入总量上限）——
    /// 上游 `File(...)` 是单值，多传不是它支持的用法；这里选「第一个生效」
    /// 而不是报错，是为了不与「客户端把 zip 又塞了一遍」这种无害的失误较劲。
    pub file: Option<ReceivedFile>,
    /// 普通文本字段（字段名 → 值）。同名字段后者覆盖前者。
    pub fields: std::collections::BTreeMap<String, String>,
}

/// [`ReceivedForm`] 里的那个文件字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivedFile {
    pub field: String,
    pub file_name: Option<String>,
    pub content_type: Option<String>,
    /// 实际写进文件的字节数。
    pub bytes_written: u64,
}

/// 把 multipart 请求体**流式**写进 `dest`，返回文本字段。
///
/// # 为什么不能复用 [`Multipart::next_file`]
///
/// `next_file` 把字段累积到 `Vec<u8>`。对 8 MiB 的封面图那是合适的（内存里
/// 有它反而更好用），而插件包的上限是 **100 MiB** —— 全缓冲意味着一次上传
/// 就占掉一台 16 GB 机器内存的 1/160，几台并发乘上去。上游是把 body
/// `copyfileobj` 到临时文件再交给安装器，这里照做。
///
/// # 总量上限是**整个请求**的，不只是文件
///
/// 文件字节 + 文本字段字节一起计入 `max_bytes`。只有文件计入的话，
/// 「一个巨大的普通表单字段」就成了绕过闸门的路 —— 而流式路径里它会被
/// `field.text()` 整个读进内存。
///
/// # `too_large_code` 是参数而不是常量
///
/// 通用提取器超限用 `http_error`；插件端点要回上游自己的 `plugin_too_large`。
/// 两者状态码相同（413）而 `code` 不同，客户端按 `code` 分支 —— 所以由调用方
/// 决定，不在这里二选一。
///
/// # 写盘失败 → 500
///
/// 磁盘满 / 权限不足是**服务端**问题，不是上传者的错（上游同样落到 500）。
pub async fn receive_to_file(
    multipart: &mut Multipart,
    dest: &Path,
    max_bytes: u64,
    too_large_code: &str,
) -> Result<ReceivedForm, ErrorResponse> {
    let mut form = ReceivedForm::default();
    let mut sink: Option<tokio::fs::File> = None;
    // 已经读进来的**总**字节数（文件 + 文本）。
    let mut total: u64 = 0;

    while let Some(mut field) = multipart
        .inner
        .next_field()
        .await
        .map_err(ErrorResponse::from)?
    {
        let name = field.name().unwrap_or_default().to_owned();
        let is_file = field.file_name().is_some();
        let keep = is_file && form.file.is_none();

        if keep {
            sink = Some(tokio::fs::File::create(dest).await.map_err(write_failure)?);
            form.file = Some(ReceivedFile {
                field: name.clone(),
                file_name: field.file_name().map(str::to_owned),
                content_type: field.content_type().map(str::to_owned),
                bytes_written: 0,
            });
        }

        let mut text: Vec<u8> = Vec::new();
        while let Some(chunk) = field.chunk().await.map_err(ErrorResponse::from)? {
            total += chunk.len() as u64;
            if total > max_bytes {
                return Err(too_large(too_large_code, max_bytes, total));
            }
            if keep {
                if let Some(file) = sink.as_mut() {
                    file.write_all(&chunk).await.map_err(write_failure)?;
                }
            } else if !is_file {
                text.extend_from_slice(&chunk);
                if text.len() > MAX_TEXT_FIELD_BYTES {
                    return Err(ErrorResponse::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "validation_error",
                        "Request validation failed",
                    )
                    .with_details(details_of_pair(
                        "field",
                        &name,
                        "max_bytes",
                        MAX_TEXT_FIELD_BYTES as i64,
                    )));
                }
            }
        }

        if !is_file {
            // 字段值未必是 UTF-8（客户端可以发二进制），坏字节按替换字符处理
            // 而不是 422：这个字段的内容由调用方去解析，报错的位置不该在这里。
            form.fields
                .insert(name, String::from_utf8_lossy(&text).into_owned());
        }
    }

    if let Some(file) = sink.as_mut() {
        file.flush().await.map_err(write_failure)?;
    }
    if let Some(found) = form.file.as_mut() {
        found.bytes_written = total;
    }
    Ok(form)
}

/// 413：超出上传上限。
fn too_large(code: &str, max_bytes: u64, received: u64) -> ErrorResponse {
    let mut details = serde_json::Map::new();
    details.insert("max_bytes".to_owned(), serde_json::Value::from(max_bytes));
    details.insert(
        "received_bytes".to_owned(),
        serde_json::Value::from(received),
    );
    ErrorResponse::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        code,
        format!("上传体超过大小上限 {max_bytes} 字节"),
    )
    .with_details(details)
}

/// 写临时文件失败 → 500（服务端问题）。
fn write_failure(error: std::io::Error) -> ErrorResponse {
    let mut details = serde_json::Map::new();
    details.insert(
        "detail".to_owned(),
        serde_json::Value::from(error.to_string()),
    );
    ErrorResponse::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        "写入临时文件失败",
    )
    .with_details(details)
}

impl<S> FromRequest<S> for Multipart
where
    S: Send + Sync,
{
    /// multipart 请求体本身不合法（缺 `Content-Type`、boundary 缺失、
    /// chunked 体损坏）→ 422 `validation_error`。
    type Rejection = ErrorResponse;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match AxumMultipart::from_request(request, state).await {
            Ok(inner) => Ok(Self::new(inner)),
            Err(rejection) => Err(ErrorResponse::from(rejection)),
        }
    }
}

impl From<axum::extract::multipart::MultipartError> for ErrorResponse {
    /// 读流失败（body 截断、chunk 编码错误）→ **422** `validation_error`。
    ///
    /// 与 [`MultipartRejection`] 同一个映射，但**理由不同**：rejection 发生在
    /// 「还没开始读」—— `Content-Type` 不对或 boundary 缺失；`MultipartError`
    /// 发生在「读到一半」—— 客户端断了。两者都不是文件本身的问题，所以都是
    /// 422 而不是 4xx 里的其它码。
    fn from(value: axum::extract::multipart::MultipartError) -> Self {
        let mut details = serde_json::Map::new();
        details.insert(
            "detail".to_owned(),
            serde_json::Value::from(value.body_text()),
        );
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            "Request validation failed",
        )
        .with_details(details)
    }
}

impl From<MultipartRejection> for ErrorResponse {
    /// 400 → **422** `validation_error`，与上游 `RequestValidationError` 对齐。
    ///
    /// 这个映射不是「顺手」：客户端对 400 与 422 的处理不同（重试策略），
    /// 而上游从来不会因为上传体损坏而回 400。
    fn from(value: MultipartRejection) -> Self {
        let mut details = serde_json::Map::new();
        details.insert(
            "detail".to_owned(),
            serde_json::Value::from(value.body_text()),
        );
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            "Request validation failed",
        )
        .with_details(details)
    }
}
