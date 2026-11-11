//! 历史回看：把同一 Principal 的交互链路当作只读文件树提供给注入的 StraviaRead。
//!
//! 唯一入口 [`read`] 按 Principal 与已解析的历史路径返回一页输出或一个错误；成员规则在
//! Observation 存储里，目录、转录、工具文件的渲染与分页、凭据替换都在本模块内部。
//! 调用方负责执行范围校验（只有透明注入的历史范围能到达这里）。

// 索引调度与检索调用分别在后续票据接入；本票先交付可直接调用的实现。
#[allow(dead_code)]
pub(crate) mod index;
mod page;
pub(crate) mod render;
// 查询解析器将在后续检索票据接入。
#[allow(dead_code)]
mod search_text;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use stravia_credential_protection::KnownSecrets;
use stravia_runtime_contract::{Principal, hook::PlatformToolError};
use stravia_web_access_contract::read_path::{HistoryNode, HistoryPath};

use crate::interaction_observation::{InteractionObservation, ObservationEvent, RecallInteraction};
use page::{ChunkMark, FileCursor, Offer, Page, Position, invalid_cursor, outside, stale_cursor};
use render::{Transcript, TranscriptState};

const UNAVAILABLE: &str =
    "History entry unavailable: it does not exist, has expired, or was cleared.";
const CHAIN_PAGE: usize = 50;
const MEMBER_PAGE: usize = 100;
const EVENT_BATCH: usize = 200;

/// 读取一个历史路径。凭据映射在每次读取时重新加载，Key 的映射变化从下一次读取起生效。
pub(crate) async fn read(
    gateway: &crate::Gateway,
    principal: &Principal,
    path: HistoryPath,
) -> Result<Value, PlatformToolError> {
    let secrets = KnownSecrets::load(&*gateway.redaction.mappings, principal)
        .await
        .map_err(|error| {
            tracing::warn!(error = %error, "history recall could not load credential mappings");
            PlatformToolError::new("History Recall could not load credential mappings")
        })?;
    let retention = gateway.retention();
    Recall {
        observation: &gateway.observation,
        secrets,
        retention_days: retention.days(),
        active_since: chrono::Utc::now()
            .timestamp_millis()
            .saturating_sub(retention.window_millis()),
    }
    .read(principal.api_key_id(), &path)
    .await
}

fn unavailable() -> PlatformToolError {
    PlatformToolError::new(UNAVAILABLE)
}

fn storage(error: anyhow::Error) -> PlatformToolError {
    tracing::warn!(error = %format!("{error:#}"), "history recall storage read failed");
    PlatformToolError::new("History Recall could not read stored history")
}

/// 目录游标：上一页最后一行的键集位置。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DirectoryCursor {
    v: u8,
    path: String,
    at: i64,
    id: String,
}

struct Recall<'a> {
    observation: &'a InteractionObservation,
    secrets: KnownSecrets,
    retention_days: u32,
    /// 最近活动时间严格大于它的 Interaction 才在回看窗口内，与到期清理互补。
    active_since: i64,
}

impl Recall<'_> {
    async fn read(&self, principal: &str, path: &HistoryPath) -> Result<Value, PlatformToolError> {
        let address = path.node.address();
        match &path.node {
            HistoryNode::Root => self.root(principal, &address, path).await,
            HistoryNode::Chain { root_id } => self.chain(principal, &address, path, root_id).await,
            HistoryNode::Transcript {
                root_id,
                interaction_id,
            } => {
                let interaction = self.interaction(principal, root_id, interaction_id).await?;
                self.transcript(&address, path, &interaction).await
            }
            HistoryNode::Tool {
                root_id,
                interaction_id,
                index,
            } => {
                let interaction = self.interaction(principal, root_id, interaction_id).await?;
                self.tool(&address, path, &interaction, *index).await
            }
        }
    }

    async fn interaction(
        &self,
        principal: &str,
        root_id: &str,
        interaction_id: &str,
    ) -> Result<RecallInteraction, PlatformToolError> {
        self.observation
            .recall_interaction(principal, root_id, interaction_id, self.active_since)
            .await
            .map_err(storage)?
            .ok_or_else(unavailable)
    }

    fn directory_after(
        address: &str,
        path: &HistoryPath,
    ) -> Result<Option<(i64, String)>, PlatformToolError> {
        let Some(encoded) = &path.cursor else {
            return Ok(None);
        };
        let cursor: DirectoryCursor = page::decode(encoded)?;
        if cursor.v != 1 || cursor.path != address {
            return Err(invalid_cursor());
        }
        Ok(Some((cursor.at, cursor.id)))
    }

    fn directory_next(
        address: &str,
        last: Option<(i64, &str)>,
    ) -> Result<Option<String>, PlatformToolError> {
        last.map(|(at, id)| {
            page::encode(&DirectoryCursor {
                v: 1,
                path: address.to_owned(),
                at,
                id: id.to_owned(),
            })
        })
        .transpose()
    }

    async fn root(
        &self,
        principal: &str,
        address: &str,
        path: &HistoryPath,
    ) -> Result<Value, PlatformToolError> {
        let after = Self::directory_after(address, path)?;
        let listing = self
            .observation
            .recall_chains(principal, self.active_since, after.as_ref(), CHAIN_PAGE)
            .await
            .map_err(storage)?;
        let heads: Vec<String> = listing
            .chains
            .iter()
            .map(|chain| chain.head_id.clone())
            .collect();
        let previews = self
            .observation
            .recall_previews(principal, &heads)
            .await
            .map_err(storage)?;
        let mut content = render::root_header(listing.total, self.retention_days);
        content.push('\n');
        for chain in &listing.chains {
            let preview = render::preview(
                previews.get(&chain.head_id).map(String::as_str),
                &self.secrets,
            );
            content.push_str(&render::chain_line(chain, &preview));
            content.push('\n');
        }
        let next = Self::directory_next(
            address,
            listing
                .chains
                .last()
                .filter(|_| listing.has_more)
                .map(|chain| (chain.last_active_at, chain.latest_id.as_str())),
        )?;
        Ok(page::output(address, content, None, next))
    }

    async fn chain(
        &self,
        principal: &str,
        address: &str,
        path: &HistoryPath,
        root_id: &str,
    ) -> Result<Value, PlatformToolError> {
        let after = Self::directory_after(address, path)?;
        let listing = self
            .observation
            .recall_members(
                principal,
                root_id,
                self.active_since,
                after.as_ref(),
                MEMBER_PAGE,
            )
            .await
            .map_err(storage)?
            .ok_or_else(unavailable)?;
        let ids: Vec<String> = listing
            .members
            .iter()
            .map(|member| member.id.clone())
            .collect();
        let previews = self
            .observation
            .recall_previews(principal, &ids)
            .await
            .map_err(storage)?;
        let mut content = render::chain_header(root_id, listing.total, listing.started_at);
        content.push('\n');
        // 分叉标记看上一行；新页的上一行就是游标里上一页的最后一行。
        let mut previous = after.as_ref().map(|(_, id)| id.as_str());
        for member in &listing.members {
            let preview =
                render::preview(previews.get(&member.id).map(String::as_str), &self.secrets);
            content.push_str(&render::member_line(root_id, member, previous, &preview));
            content.push('\n');
            previous = Some(&member.id);
        }
        let next = Self::directory_next(
            address,
            listing
                .members
                .last()
                .filter(|_| listing.has_more)
                .map(|member| (member.started_at, member.id.as_str())),
        )?;
        Ok(page::output(address, content, None, next))
    }

    async fn tool(
        &self,
        address: &str,
        path: &HistoryPath,
        interaction: &RecallInteraction,
        index: u32,
    ) -> Result<Value, PlatformToolError> {
        let tool = self
            .observation
            .recall_tool(&interaction.id, index)
            .await
            .map_err(storage)?
            .ok_or_else(unavailable)?;
        let lines = render::tool_file(interaction, index, &tool, &self.secrets);
        let total = lines.len() as u64;
        let position = match &path.cursor {
            Some(encoded) => {
                let cursor: FileCursor = page::decode(encoded)?;
                if cursor.v != 1 || cursor.path != address || cursor.chunk.is_some() {
                    return Err(invalid_cursor());
                }
                cursor.position.validate()?;
                if cursor.position.line > total {
                    return Err(stale_cursor());
                }
                cursor.position
            }
            None => {
                let position = Position::start(page::resolve(path.lines.as_deref(), Some(total))?);
                if position.last_start() > total {
                    return Err(outside());
                }
                position
            }
        };
        let mut page = Page::new(position);
        let (resume_line, resume_offset) = page.resume_at();
        let skip = usize::try_from(resume_line.saturating_sub(1)).unwrap_or(usize::MAX);
        for (number, line) in (resume_line..).zip(lines.iter().skip(skip)) {
            let from = if number == resume_line {
                resume_offset
            } else {
                0
            };
            if let Offer::Finished = page.offer(number, line, from)? {
                break;
            }
        }
        let (content, ranges, next) = page.finish();
        let next = next
            .map(|position| {
                page::encode(&FileCursor {
                    v: 1,
                    path: address.to_owned(),
                    position,
                    chunk: None,
                })
            })
            .transpose()?;
        Ok(page::output(address, content, Some(ranges), next))
    }

    async fn transcript(
        &self,
        address: &str,
        path: &HistoryPath,
        interaction: &RecallInteraction,
    ) -> Result<Value, PlatformToolError> {
        let transcript = Transcript::new(interaction, &self.secrets);
        let (position, mark, check_through) = match &path.cursor {
            Some(encoded) => {
                let cursor: FileCursor = page::decode(encoded)?;
                let mark = match cursor.chunk {
                    Some(mark) if cursor.v == 1 && cursor.path == address => mark,
                    _ => return Err(invalid_cursor()),
                };
                cursor.position.validate()?;
                mark.validate(interaction, cursor.position.line)?;
                (cursor.position, mark, 0)
            }
            None => {
                let total = if page::needs_total(path.lines.as_deref()) {
                    Some(self.count_lines(transcript, &interaction.id).await?)
                } else {
                    None
                };
                let position = Position::start(page::resolve(path.lines.as_deref(), total)?);
                let check_through = position.last_start();
                (position, ChunkMark::header(), check_through)
            }
        };
        let mut walker = self.walker(transcript, &interaction.id, &mark).await?;
        let mut page = Page::new(position);
        let (resume_line, resume_offset) = page.resume_at();
        let mut stopped_in = None;
        let mut last_line = 0;
        while let Some(chunk) = walker.next().await? {
            last_line = chunk.mark.line + chunk.lines.len() as u64 - 1;
            if !page.finished() && last_line >= resume_line {
                for (number, line) in (chunk.mark.line..).zip(&chunk.lines) {
                    if number < resume_line {
                        continue;
                    }
                    let from = if number == resume_line {
                        resume_offset
                    } else {
                        0
                    };
                    if let Offer::Finished = page.offer(number, line, from)? {
                        stopped_in = Some(chunk.mark.clone());
                        break;
                    }
                }
            }
            // 初次读取要确认每个选择的起点都在转录内，页满后仍只数行到最后一个起点。
            if page.finished() && last_line >= check_through {
                break;
            }
        }
        if last_line < check_through {
            return Err(outside());
        }
        if last_line < resume_line {
            return Err(stale_cursor());
        }
        let (content, ranges, next) = page.finish();
        let next = match (next, stopped_in) {
            (Some(position), Some(chunk)) => Some(page::encode(&FileCursor {
                v: 1,
                path: address.to_owned(),
                position,
                chunk: Some(chunk),
            })?),
            _ => None,
        };
        Ok(page::output(address, content, Some(ranges), next))
    }

    /// `:-K` 需要总行数：完整渲染一遍只计数。
    async fn count_lines(
        &self,
        transcript: Transcript<'_>,
        interaction_id: &str,
    ) -> Result<u64, PlatformToolError> {
        let mut walker = self
            .walker(transcript, interaction_id, &ChunkMark::header())
            .await?;
        let mut total = 0;
        while let Some(chunk) = walker.next().await? {
            total += chunk.lines.len() as u64;
        }
        Ok(total)
    }

    /// 从游标记录的块开始渲染：工具编号由此前的工具开始事件重建，上一个 Run 取自游标。
    async fn walker<'w>(
        &'w self,
        transcript: Transcript<'w>,
        interaction_id: &'w str,
        mark: &ChunkMark,
    ) -> Result<Walker<'w>, PlatformToolError> {
        let Some(event) = mark.event else {
            return Ok(Walker {
                observation: self.observation,
                transcript,
                interaction_id,
                state: TranscriptState::default(),
                next_line: 1,
                header_pending: true,
                after: i64::MIN,
                expect_first: None,
                buffer: Vec::new().into_iter(),
                exhausted: false,
            });
        };
        let tool_ids = self
            .observation
            .recall_tool_ids(interaction_id, event)
            .await
            .map_err(storage)?;
        if tool_ids.len() != mark.tools as usize {
            return Err(stale_cursor());
        }
        Ok(Walker {
            observation: self.observation,
            transcript,
            interaction_id,
            state: TranscriptState::resume(tool_ids, mark.run.clone()),
            next_line: mark.line,
            header_pending: false,
            after: event.saturating_sub(1),
            expect_first: Some(event),
            buffer: Vec::new().into_iter(),
            exhausted: false,
        })
    }
}

/// 转录的一个渲染块（头部或一个事件）及其起点。
struct Chunk {
    mark: ChunkMark,
    lines: Vec<String>,
}

/// 按事件序号分批读取并渲染转录；不渲染的事件不产生块。
struct Walker<'w> {
    observation: &'w InteractionObservation,
    transcript: Transcript<'w>,
    interaction_id: &'w str,
    state: TranscriptState,
    next_line: u64,
    header_pending: bool,
    after: i64,
    /// 续读时第一个事件必须是游标记录的事件，否则游标已失效。
    expect_first: Option<i64>,
    buffer: std::vec::IntoIter<ObservationEvent>,
    exhausted: bool,
}

impl Walker<'_> {
    async fn next(&mut self) -> Result<Option<Chunk>, PlatformToolError> {
        if self.header_pending {
            self.header_pending = false;
            return Ok(Some(self.chunk(None, self.transcript.header())));
        }
        loop {
            let Some(event) = self.buffer.next() else {
                if self.exhausted {
                    return match self.expect_first {
                        Some(_) => Err(stale_cursor()),
                        None => Ok(None),
                    };
                }
                let batch = self
                    .observation
                    .recall_events(self.interaction_id, self.after, EVENT_BATCH)
                    .await
                    .map_err(storage)?;
                self.exhausted = batch.len() < EVENT_BATCH;
                self.buffer = batch.into_iter();
                continue;
            };
            self.after = event.sequence;
            if let Some(expected) = self.expect_first.take()
                && event.sequence != expected
            {
                return Err(stale_cursor());
            }
            let before = (self.state.allocated(), self.state.run().map(str::to_owned));
            let lines = self.transcript.event(&mut self.state, &event);
            if lines.is_empty() {
                continue;
            }
            let mut chunk = self.chunk(Some(event.sequence), lines);
            (chunk.mark.tools, chunk.mark.run) = before;
            return Ok(Some(chunk));
        }
    }

    fn chunk(&mut self, event: Option<i64>, lines: Vec<String>) -> Chunk {
        let mark = ChunkMark {
            event,
            line: self.next_line,
            tools: 0,
            run: None,
        };
        self.next_line += lines.len() as u64;
        Chunk { mark, lines }
    }
}
