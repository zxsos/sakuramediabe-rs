//! 三个仓储的集成测试（L2 验证）。
//!
//! # 全部会在没有 PostgreSQL 时跳过
//!
//! 每个测试开头是 `let Some(db) = TestDb::create().await else { return };`
//! —— 拿不到连接就直接返回，测试算**通过**。这样 `cargo test` 在没起
//! 库的环境里仍然是全绿，而起了库就会真的跑。
//!
//! 跑之前需要：
//!
//! ```text
//! python parity/apply_ddl.py            # 起库 + 建表
//! $env:SMDB_TEST_DATABASE_URL = "postgres://sakuramedia:...@localhost:5433/sakuramedia_test"
//! cargo test -p sm-db --test repo_integration -- --nocapture
//! ```
//!
//! # 覆盖的七件事
//!
//! | # | 验证内容 | 为什么重要 |
//! |---|---|---|
//! | 1 | JSONB 往返 | `metadata_source` / `field_owners` 是 jsonb 列 |
//! | 2 | 服务端 DEFAULT 生效 | `field_owners` 默认 `{}`、`mutation_revision` 默认 0 |
//! | 3 | CHECK 生效 | 直接写 SQL 绕过仓储，确认数据库真的拦 |
//! | 4 | XOR 拦截 | 归属不变量返回 422 而非外键错误 |
//! | 5 | `updated_at` 自动推进 | 只改一个字段，时间戳也必须变 |
//! | 6 | 双状态机独立 | 改一侧不影响另一侧 |
//! | 7 | 字段护栏 | 插件写 `field_owners` 被拒 |

use sm_db::common::guard::WriteSource;
use sm_db::common::update::UpdateSet;
use sm_db::error::DbError;
use sm_db::repo::{DownloadTaskRepository, MediaRepository, MovieRepository, NewDownloadTask};
use sm_db::testing::TestDb;

mod fixtures {
    use sm_db::repo::NewMovie;

    pub fn movie(number: &str) -> NewMovie {
        NewMovie {
            movie_number: number.to_owned(),
            title: format!("{number} 标题"),
            ..NewMovie::default()
        }
    }

    pub fn media_for_movie(number: &str) -> sm_db::repo::media::NewMedia {
        sm_db::repo::media::NewMedia {
            library_id: 1,
            file_name: format!("{number}.mp4"),
            file_size_bytes: 1024,
            movie_number: Some(number.to_owned()),
            video_item_id: None,
            storage_ref: None,
            resolution: Some("1080p".to_owned()),
            file_hash: None,
            import_source_identity: None,
            duration_seconds: Some(120),
            video_info: None,
        }
    }
}

// ---------------------------------------------------------------- JSONB

#[tokio::test]
async fn jsonb_columns_roundtrip() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());

    let created = repo
        .insert(&fixtures::movie("ABC-001"))
        .await
        .expect("插入失败");

    // metadata_source 是 jsonb，插入后应原样读回。
    let payload = serde_json::json!({"provider": "local", "version": 2});
    let mut set = UpdateSet::new();
    set.set("metadata_source", payload.clone());
    let updated = repo
        .update(created.id, set, WriteSource::Host)
        .await
        .expect("更新失败");

    assert_eq!(updated.metadata_source, Some(payload.clone()));
    assert_eq!(
        repo.require_by_id(created.id)
            .await
            .unwrap()
            .metadata_source,
        Some(payload)
    );
}

#[tokio::test]
async fn jsonb_text_column_tolerates_null_and_roundtrips() {
    // JsonTextField 是 TEXT 列装 JSON —— 与真 jsonb 列是两种东西。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MediaRepository::new(db.pool().clone());

    let mut m = fixtures::media_for_movie("ABC-002");
    m.video_info = Some(serde_json::json!({"codec": "h264", "bitrate": 4500}));
    let created = repo.insert(&m).await.expect("插入失败");

    // 读出来是 Option<String>（TEXT 列），需要显式解析。
    let raw = created.video_info.as_deref();
    assert!(raw.is_some_and(|s| s.contains("h264")), "got {raw:?}");
    let parsed = sm_db::common::json_text::decode(raw);
    assert_eq!(parsed.unwrap()["codec"], "h264");
}

#[tokio::test]
async fn null_jsonb_stays_null_rather_than_becoming_empty_object() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());

    let created = repo.insert(&fixtures::movie("ABC-003")).await.unwrap();
    assert!(
        created.metadata_source.is_none(),
        "未指定时应为 NULL，不能是 {{}}"
    );
}

// ---------------------------------------------------------------- DEFAULT

#[tokio::test]
async fn server_side_defaults_apply_on_bare_insert() {
    // 仓储的 insert 不写这两列，值必须来自 schema 的 DEFAULT。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());

    let created = repo.insert(&fixtures::movie("ABC-004")).await.unwrap();
    assert_eq!(
        created.field_owners,
        serde_json::json!({}),
        "field_owners 的 DEFAULT 是 {{}}"
    );
    assert_eq!(created.mutation_revision, 0, "mutation_revision DEFAULT 0");
    assert_eq!(created.heat, 0);
    assert_eq!(created.watched_count, 0);
    assert!(!created.is_subscribed);
    assert!(!created.is_blacklisted);
}

#[tokio::test]
async fn download_task_defaults_come_from_schema() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = DownloadTaskRepository::new(db.pool().clone());

    let created = repo
        .insert(&NewDownloadTask {
            client_id: 1,
            remote_id: "remote-1".to_owned(),
            name: "任务".to_owned(),
            movie_number: None,
        })
        .await
        .expect("插入失败");

    assert_eq!(created.state, "queued", "DEFAULT 'queued'");
    assert_eq!(created.import_status, "pending", "DEFAULT 'pending'");
    assert_eq!(created.progress, 0.0);
    // movie_number 可空：任务允许早于影片入库。
    assert!(created.movie_number.is_none());
}

// ---------------------------------------------------------------- CHECK

#[tokio::test]
async fn database_rejects_subscribed_and_blacklisted_together() {
    // 绕过仓储直接写 SQL，确认 CHECK 真的存在于 schema 里。
    // 仓储层会提前拦（返回 422），但数据库必须是最后一道防线。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let created = repo.insert(&fixtures::movie("ABC-005")).await.unwrap();

    let result =
        sqlx::query("UPDATE movie SET is_subscribed = true, is_blacklisted = true WHERE id = $1")
            .bind(created.id)
            .execute(db.pool())
            .await;

    let err = result.expect_err("CHECK 约束必须拒绝同时为真");
    let db_err = err.as_database_error().expect("应是数据库错误");
    assert_eq!(
        db_err.code().as_deref(),
        Some("23514"),
        "SQLSTATE 应为 check_violation"
    );

    // 经 DbError 转换后应归类为 ConstraintViolation（409），不是 Business（422）。
    let classified = err.into();
    assert!(
        matches!(classified, DbError::ConstraintViolation { .. }),
        "绕过仓储的写入应由数据库兜底并归为 409"
    );
}

#[tokio::test]
async fn repository_precheck_returns_422_before_hitting_the_database() {
    // 同一场景走仓储：应返回 Business(422) 并带原因，而不是 409。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let created = repo.insert(&fixtures::movie("ABC-006")).await.unwrap();

    let mut set = UpdateSet::new();
    set.set("is_subscribed", true);
    repo.update(created.id, set, WriteSource::Host)
        .await
        .expect("单独订阅应成功");

    // 再叠加屏蔽 -> 冲突。预判要读**当前**的 is_subscribed 才能发现。
    let mut set = UpdateSet::new();
    set.set("is_blacklisted", true);
    let err = repo
        .update(created.id, set, WriteSource::Host)
        .await
        .expect_err("必须被预判拦下");

    match err {
        DbError::Business { entity, reason } => {
            assert_eq!(entity, "Movie");
            assert!(reason.contains("is_subscribed"), "{reason}");
        }
        other => panic!("应为 Business(422)，实际 {other:?}"),
    }
}

// ---------------------------------------------------------------- XOR

#[tokio::test]
async fn media_rejects_both_parents_present() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MediaRepository::new(db.pool().clone());

    let mut m = fixtures::media_for_movie("ABC-007");
    m.video_item_id = Some(1); // 两者都非空
    let err = repo.insert(&m).await.expect_err("XOR 不变量必须拒绝");

    match err {
        DbError::Business { entity, reason } => {
            assert_eq!(entity, "Media");
            assert!(reason.contains("恰好归属"), "{reason}");
        }
        other => panic!("应为 Business(422)，实际 {other:?}"),
    }
}

#[tokio::test]
async fn media_rejects_no_parent() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MediaRepository::new(db.pool().clone());

    let mut m = fixtures::media_for_movie("ABC-008");
    m.movie_number = None; // 两者都空
    m.video_item_id = None;
    assert!(repo.insert(&m).await.is_err(), "无归属的 Media 必须被拒绝");
}

#[tokio::test]
async fn media_accepts_either_parent() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MediaRepository::new(db.pool().clone());

    // 挂 movie（JAV）
    let jav = repo
        .insert(&fixtures::media_for_movie("ABC-009"))
        .await
        .expect("movie 归属应被接受");
    assert!(jav.satisfies_owner_constraint());
    assert!(jav.movie_number.is_some());

    // 挂 video_item（非 JAV）
    let mut non_jav = fixtures::media_for_movie("ABC-010");
    non_jav.movie_number = None;
    non_jav.video_item_id = Some(42);
    let created = repo
        .insert(&non_jav)
        .await
        .expect("video_item 归属应被接受");
    assert!(created.satisfies_owner_constraint());
    assert_eq!(created.video_item_id, Some(42));
}

#[tokio::test]
async fn media_update_prechecks_the_xor_invariant() {
    // 改归属列时也要预判，否则错误会以 FK 冲突形式漏出。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MediaRepository::new(db.pool().clone());
    let created = repo
        .insert(&fixtures::media_for_movie("ABC-011"))
        .await
        .unwrap();

    // 现在挂着 movie_number，同时写 video_item_id -> 冲突。
    let mut set = UpdateSet::new();
    set.set("video_item_id", 7i64);
    let err = repo.update(created.id, set).await.expect_err("必须预判");
    assert!(matches!(err, DbError::Business { .. }), "实际 {err:?}");
}

// ---------------------------------------------------------------- updated_at

#[tokio::test]
async fn updated_at_advances_on_every_update() {
    // 这是本仓储层存在的核心理由：上游曾因 peewee 的 default= 只在
    // INSERT 生效，导致五处「最近修改优先」列表静默排成创建顺序。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let created = repo.insert(&fixtures::movie("ABC-012")).await.unwrap();

    let first = created.updated_at.expect("插入时应有 updated_at");

    // PostgreSQL 的 timestamp 微秒精度，等一会儿再改。
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let mut set = UpdateSet::new();
    set.set("title", "改过的标题");
    let updated = repo
        .update(created.id, set, WriteSource::Host)
        .await
        .expect("更新失败");

    let second = updated.updated_at.expect("更新后应有 updated_at");
    assert!(second > first, "updated_at 必须推进：{first} -> {second}");
    assert!(second - first >= chrono::Duration::seconds(1));
}

#[tokio::test]
async fn updated_at_advances_even_when_caller_tries_to_pin_it() {
    // 调用方即使自己写了 updated_at，Repository 也会覆盖。
    let repo_pin = chrono::NaiveDate::from_ymd_opt(2000, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();

    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let created = repo.insert(&fixtures::movie("ABC-013")).await.unwrap();

    let mut set = UpdateSet::new();
    set.set("title", "x");
    set.set("updated_at", repo_pin); // 试图钉住时间戳
    let updated = repo
        .update(created.id, set, WriteSource::Host)
        .await
        .unwrap();

    assert_ne!(
        updated.updated_at,
        Some(repo_pin),
        "Repository 强制 touch，调用方钉不住"
    );
}

#[tokio::test]
async fn empty_update_is_rejected_instead_of_reporting_not_found() {
    // 空 UPDATE 的 rows_affected = 0，与「行不存在」无法区分。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let created = repo.insert(&fixtures::movie("ABC-014")).await.unwrap();

    let err = repo
        .update(created.id, UpdateSet::new(), WriteSource::Host)
        .await
        .expect_err("空更新应被拒绝");

    assert!(matches!(err, DbError::Business { .. }));
}

// ---------------------------------------------------------------- 双状态机

#[tokio::test]
async fn the_two_state_machines_move_independently() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = DownloadTaskRepository::new(db.pool().clone());
    let created = repo
        .insert(&NewDownloadTask {
            client_id: 1,
            remote_id: "remote-2".to_owned(),
            name: "任务".to_owned(),
            movie_number: Some("ABC-015".to_owned()),
        })
        .await
        .unwrap();

    // 只动远端状态
    let after = repo
        .set_state(created.id, "downloading", Some(0.5), None)
        .await
        .expect("设置远端状态失败");
    assert_eq!(after.state, "downloading");
    assert_eq!(after.import_status, "pending", "导入状态不应被远端状态影响");
    assert_eq!(after.progress, 0.5);

    // 只动导入状态
    let after = repo
        .set_import_status(created.id, "running", None)
        .await
        .expect("设置导入状态失败");
    assert_eq!(after.import_status, "running");
    assert_eq!(after.state, "downloading", "远端状态不应被导入状态影响");
}

#[tokio::test]
async fn completed_state_requires_a_source_ref() {
    // 完成态没有产物引用，导入侧无从下手。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = DownloadTaskRepository::new(db.pool().clone());
    let created = repo
        .insert(&NewDownloadTask {
            client_id: 1,
            remote_id: "remote-3".to_owned(),
            name: "任务".to_owned(),
            movie_number: None,
        })
        .await
        .unwrap();

    let err = repo
        .set_state(created.id, "completed", Some(1.0), None)
        .await
        .expect_err("缺少 source_ref 应被拒绝");
    assert!(matches!(err, DbError::Business { .. }), "实际 {err:?}");
}

#[tokio::test]
async fn download_done_but_import_failed_is_expressible_and_listable() {
    // 这个组合只有把两个状态机分开才表达得了。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = DownloadTaskRepository::new(db.pool().clone());
    let created = repo
        .insert(&NewDownloadTask {
            client_id: 1,
            remote_id: "remote-4".to_owned(),
            name: "任务".to_owned(),
            movie_number: None,
        })
        .await
        .unwrap();

    repo.set_state(created.id, "completed", Some(1.0), Some("ref://x"))
        .await
        .unwrap();
    let stuck = repo
        .set_import_status(created.id, "failed", None)
        .await
        .unwrap();

    assert!(stuck.download_finished());
    assert!(stuck.import_finished());
    assert!(stuck.fully_settled());
    assert!(stuck.is_stuck_after_download());

    let listed = repo.list_stuck_after_download().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.id);
}

#[tokio::test]
async fn progress_outside_unit_range_is_rejected() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = DownloadTaskRepository::new(db.pool().clone());
    let created = repo
        .insert(&NewDownloadTask {
            client_id: 1,
            remote_id: "remote-5".to_owned(),
            name: "任务".to_owned(),
            movie_number: None,
        })
        .await
        .unwrap();

    for bad in [-0.1, 1.1] {
        let err = repo
            .set_state(created.id, "downloading", Some(bad), None)
            .await;
        assert!(err.is_err(), "progress={bad} 应被拒绝");
    }
}

#[tokio::test]
async fn unknown_state_literals_are_rejected() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = DownloadTaskRepository::new(db.pool().clone());
    let created = repo
        .insert(&NewDownloadTask {
            client_id: 1,
            remote_id: "remote-6".to_owned(),
            name: "任务".to_owned(),
            movie_number: None,
        })
        .await
        .unwrap();

    let err = repo
        .set_state(created.id, "teleporting", Some(0.1), None)
        .await
        .expect_err("未知状态应被拒绝");
    assert!(err.to_string().contains("teleporting"), "{err}");
}

// ---------------------------------------------------------------- 护栏

#[tokio::test]
async fn guard_blocks_plugin_from_writing_host_only_columns() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let created = repo.insert(&fixtures::movie("ABC-016")).await.unwrap();

    // field_owners 是 host-only：插件写了就能伪造「这字段归我所有」。
    let mut set = UpdateSet::new();
    set.set("field_owners", serde_json::json!({"title": "plugin:evil"}));
    let err = repo
        .update(created.id, set, WriteSource::Plugin { plugin_id: "evil" })
        .await
        .expect_err("插件写 field_owners 必须被拒");

    match err {
        DbError::Business { reason, .. } => {
            assert!(reason.contains("field_owners"), "{reason}");
            assert!(reason.contains("插件 evil"), "{reason}");
        }
        other => panic!("应为 Business，实际 {other:?}"),
    }
}

#[tokio::test]
async fn guard_allows_plugin_to_write_protected_fields() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let created = repo.insert(&fixtures::movie("ABC-017")).await.unwrap();

    // title 在受保护白名单里，插件可写。
    let mut set = UpdateSet::new();
    set.set("title", "插件写的标题");
    let updated = repo
        .update(
            created.id,
            set,
            WriteSource::Plugin {
                plugin_id: "actor-metadata",
            },
        )
        .await
        .expect("插件写受保护字段应被允许");
    assert_eq!(updated.title, "插件写的标题");
}

#[tokio::test]
async fn field_owner_can_only_be_registered_for_protected_fields() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let created = repo.insert(&fixtures::movie("ABC-018")).await.unwrap();

    // watched_count 不在白名单里，登记归属应被拒。
    let err = repo
        .set_field_owner(created.id, "watched_count", "plugin:x")
        .await
        .expect_err("非受保护字段不应登记归属");
    assert!(err.to_string().contains("watched_count"), "{err}");

    // 受保护字段 + 合法 owner 格式则通过
    let updated = repo
        .set_field_owner(created.id, "title", "plugin:actor-metadata")
        .await
        .expect("登记归属应成功");
    assert_eq!(updated.field_owners["title"], "plugin:actor-metadata");
    assert_eq!(updated.mutation_revision, 1, "归属变更应递增版本号");
}

#[tokio::test]
async fn field_owner_rejects_malformed_owner_tag() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let created = repo.insert(&fixtures::movie("ABC-019")).await.unwrap();

    for bad in ["someone", "", "PLUGIN:x"] {
        let err = repo.set_field_owner(created.id, "title", bad).await;
        assert!(err.is_err(), "owner={bad:?} 应被拒绝");
    }
}

// ---------------------------------------------------------------- 缩略图状态机

#[tokio::test]
async fn thumbnail_failure_is_retryable_and_success_is_terminal() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MediaRepository::new(db.pool().clone());
    let created = repo
        .insert(&fixtures::media_for_movie("ABC-020"))
        .await
        .unwrap();

    assert_eq!(created.thumbnail_generation_state, "pending");
    assert!(!created.thumbnail_retryable());

    // 失败 -> retry_wait（可重试）
    let retry_at = sm_db::common::time::now_utc() + chrono::Duration::minutes(5);
    let failed = repo
        .record_thumbnail_failure(created.id, "storage_unavailable", retry_at)
        .await
        .unwrap();
    assert_eq!(failed.thumbnail_generation_state, "retry_wait");
    assert!(failed.thumbnail_retryable());
    assert!(!failed.thumbnail_is_terminal());
    assert_eq!(failed.thumbnail_attempt_count, 1);
    assert_eq!(
        failed.thumbnail_last_error_code.as_deref(),
        Some("storage_unavailable")
    );

    // 成功 -> succeeded（终态）
    let ok = repo.record_thumbnail_success(created.id).await.unwrap();
    assert_eq!(ok.thumbnail_generation_state, "succeeded");
    assert!(ok.thumbnail_is_terminal());
    assert!(!ok.thumbnail_retryable());
    assert!(ok.thumbnail_next_retry_at.is_none());
    assert!(ok.thumbnail_terminal_at.is_some());
}

#[tokio::test]
async fn pending_thumbnail_scan_only_returns_expired_retries() {
    // 索引是 (state, next_retry_at)，所以只有 retry_wait 且到期才被命中。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MediaRepository::new(db.pool().clone());

    let now = sm_db::common::time::now_utc();

    // 到期重试
    let expired = repo
        .insert(&fixtures::media_for_movie("ABC-021"))
        .await
        .unwrap();
    repo.record_thumbnail_failure(expired.id, "boom", now - chrono::Duration::minutes(1))
        .await
        .unwrap();

    // 未到期重试
    let pending = repo
        .insert(&fixtures::media_for_movie("ABC-022"))
        .await
        .unwrap();
    repo.record_thumbnail_failure(pending.id, "boom", now + chrono::Duration::hours(1))
        .await
        .unwrap();

    // 仍是 pending 态（从未失败过）
    let fresh = repo
        .insert(&fixtures::media_for_movie("ABC-023"))
        .await
        .unwrap();

    let due = repo.list_pending_thumbnails(50).await.unwrap();
    let ids: Vec<i32> = due.iter().map(|m| m.id).collect();

    assert!(ids.contains(&expired.id), "到期重试应被扫到");
    assert!(!ids.contains(&pending.id), "未到期不应被扫到");
    assert!(
        !ids.contains(&fresh.id),
        "pending 态靠插入初始化，不靠退避扫描"
    );
}

// ---------------------------------------------------------------- 查/更新路径

#[tokio::test]
async fn find_by_number_is_the_business_key() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    repo.insert(&fixtures::movie("ABC-024")).await.unwrap();

    let found = repo.find_by_number("ABC-024").await.unwrap();
    assert!(found.is_some());
    assert_eq!(found.unwrap().title, "ABC-024 标题");
    assert!(repo.find_by_number("NOPE-999").await.unwrap().is_none());
}

#[tokio::test]
async fn javdb_id_blank_is_normalised_to_null() {
    // 空串会让 `WHERE javdb_id = ''` 命中一条「没有编号」的假记录。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());

    let mut m = fixtures::movie("ABC-025");
    m.javdb_id = Some("   ".to_owned());
    let created = repo.insert(&m).await.unwrap();
    assert!(created.javdb_id.is_none(), "空白 javdb_id 应归一为 NULL");
}

#[tokio::test]
async fn duplicate_movie_number_hits_the_unique_constraint() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    repo.insert(&fixtures::movie("ABC-026")).await.unwrap();

    let err = repo
        .insert(&fixtures::movie("ABC-026"))
        .await
        .expect_err("番号唯一约束应拒绝重复");
    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "唯一键冲突应归为 409，实际 {err:?}"
    );
}

#[tokio::test]
async fn update_of_missing_row_reports_not_found() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());

    let mut set = UpdateSet::new();
    set.set("title", "x");
    let err = repo
        .update(999_999, set, WriteSource::Host)
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::NotFound { .. }), "实际 {err:?}");
}

#[tokio::test]
async fn claim_queued_moves_task_to_submitted() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = DownloadTaskRepository::new(db.pool().clone());

    assert!(repo.claim_queued().await.unwrap().is_none(), "空队列领不到");

    let created = repo
        .insert(&NewDownloadTask {
            client_id: 1,
            remote_id: "remote-7".to_owned(),
            name: "任务".to_owned(),
            movie_number: None,
        })
        .await
        .unwrap();
    assert_eq!(created.state, "queued");

    let claimed = repo.claim_queued().await.unwrap().expect("应领到任务");
    assert_eq!(claimed.id, created.id);
    assert_eq!(claimed.state, "submitted");

    // 已被领走，再领一次拿不到
    assert!(repo.claim_queued().await.unwrap().is_none());
}

#[tokio::test]
async fn idempotent_submit_relies_on_the_unique_index() {
    // (client, remote_id) 唯一 -> 重复提交命中约束而非产生第二条。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = DownloadTaskRepository::new(db.pool().clone());
    let payload = || NewDownloadTask {
        client_id: 1,
        remote_id: "remote-8".to_owned(),
        name: "任务".to_owned(),
        movie_number: None,
    };

    repo.insert(&payload()).await.unwrap();
    let err = repo
        .insert(&payload())
        .await
        .expect_err("重复提交应命中唯一索引");
    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "实际 {err:?}"
    );
}
