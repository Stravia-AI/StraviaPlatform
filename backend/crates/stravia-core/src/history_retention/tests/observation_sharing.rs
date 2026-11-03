//! 接缝 1：Turn Chain、History Marker 与 Observation 共享同一份正文。
//!
//! 写入走三方各自的领域入口，读取走领域读取 API，时间只经 `sweep_history(now)` 推进。
//! 「只存一份」是存储不变量，行为上无法区分，所以这里按唯一长度数正文行（跨全部 scope），
//! 与本模块的不变量测试同一做法。

use super::{Db, Fixture, Refs, run_postgres, run_sqlite};

impl Fixture {
    /// 某个文本正文在全部 scope 里的行数；用唯一长度标识一份正文。
    async fn text_rows_in_any_scope(&self, raw_len: usize) -> i64 {
        let raw_len = i64::try_from(raw_len).unwrap();
        match &self.db {
            Db::Sqlite(pool) => {
                sqlx::query_scalar("SELECT COUNT(*) FROM contents WHERE format = 0 AND raw_len = ?")
                    .bind(raw_len)
                    .fetch_one(pool)
                    .await
                    .expect("content rows")
            }
            Db::Postgres(pool) => sqlx::query_scalar(
                "SELECT COUNT(*) FROM contents WHERE format = 0 AND raw_len = $1",
            )
            .bind(raw_len)
            .fetch_one(pool)
            .await
            .expect("content rows"),
        }
    }
}

/// 同一段文字经 Observation（用户输入）、Turn Chain（响应正文）与 History Marker
/// （思考正文）写入，只落一行正文；Observation 先过期时另两方仍可读，
/// 最后一个引用方消失后由扫描回收。
async fn three_parties_share_one_body(fx: Fixture) {
    let n = fx.window();
    let shared = "three party shared body ".repeat(33);
    let visible = "three party visible body ".repeat(31);
    let tool_output = "three party tool output ".repeat(29);

    fx.observe(&shared, &visible, &tool_output).await;
    let t_observed = fx.tick().await;
    let node = fx.commit(None, &Refs::answer(&shared)).await;
    let marker = fx.thinking_marker(&shared, true).await;
    let t_written = fx.tick().await;

    assert!(fx.observed_interactions().await > 0, "interaction is readable");
    assert_eq!(
        fx.text_rows_in_any_scope(shared.len()).await,
        1,
        "Observation, Turn Chain and History Marker must share one body row"
    );
    assert_eq!(
        fx.text_body_rows(shared.len()).await,
        1,
        "the shared body lives in the principal's continuation-key scope"
    );

    fx.sweep(t_observed + n).await;
    assert_eq!(fx.observed_interactions().await, 0);
    assert!(fx.turn_mentions(&node, &shared).await);
    assert_eq!(
        fx.marker_text(&marker).await.as_deref(),
        Some(shared.as_str())
    );
    assert_eq!(fx.text_rows_in_any_scope(shared.len()).await, 1);
    assert_eq!(fx.text_rows_in_any_scope(visible.len()).await, 0);
    assert_eq!(fx.text_rows_in_any_scope(tool_output.len()).await, 0);

    fx.sweep(t_written + n).await;
    assert!(!fx.turn_readable(&node).await);
    assert!(fx.marker_text(&marker).await.is_none());
    assert_eq!(
        fx.text_rows_in_any_scope(shared.len()).await,
        0,
        "last referrer removed"
    );
}
both_backends!(
    sqlite_three_parties_share_one_body,
    postgres_three_parties_share_one_body,
    1,
    three_parties_share_one_body
);

/// 清空 Observation 历史后，只被 Observation 引用的正文由下一轮扫描回收；
/// 仍被 Turn Chain 引用的正文继续可读。
async fn clearing_history_releases_observation_only_bodies(fx: Fixture) {
    let shared = "clear shared body ".repeat(37);
    let visible = "clear visible body ".repeat(35);
    let tool_output = "clear tool output ".repeat(33);

    let node = fx.commit(None, &Refs::answer(&shared)).await;
    fx.observe(&shared, &visible, &tool_output).await;
    assert!(fx.observed_interactions().await > 0, "interaction is readable");
    assert_eq!(fx.text_rows_in_any_scope(shared.len()).await, 1);
    assert_eq!(fx.text_rows_in_any_scope(visible.len()).await, 1);
    assert_eq!(fx.text_rows_in_any_scope(tool_output.len()).await, 1);

    fx.maintenance
        .observation
        .clear_history()
        .await
        .expect("clear observation history");
    assert_eq!(fx.observed_interactions().await, 0);

    fx.sweep(super::now_ms()).await;
    assert_eq!(
        fx.text_rows_in_any_scope(visible.len()).await,
        0,
        "Observation-only visible output is collected after clear"
    );
    assert_eq!(
        fx.text_rows_in_any_scope(tool_output.len()).await,
        0,
        "Observation-only tool output is collected after clear"
    );
    assert!(
        fx.turn_mentions(&node, &shared).await,
        "the Turn Chain still reads its body"
    );
    assert_eq!(fx.text_rows_in_any_scope(shared.len()).await, 1);
}
both_backends!(
    sqlite_clearing_history_releases_observation_only_bodies,
    postgres_clearing_history_releases_observation_only_bodies,
    1,
    clearing_history_releases_observation_only_bodies
);
