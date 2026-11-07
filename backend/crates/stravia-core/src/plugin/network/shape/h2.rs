use std::{collections::HashSet, time::Duration};

use rama_http_types::proto::h2::{
    PseudoHeader, PseudoHeaderOrder,
    frame::{
        EarlyFrame, Priority, SettingId, SettingOrder, SettingsConfig, StreamDependency, StreamId,
        WindowUpdate,
    },
};

use super::super::transport::H2Settings;
use super::{EarlyFrameShape, Http2Shape, ShapeError};

const MAX_STREAM_ID: u32 = 0x7fff_ffff;

pub(super) fn resolve(shape: &Http2Shape) -> Result<H2Settings, ShapeError> {
    let mut result = H2Settings::default();

    if !shape.settings.is_empty() {
        let mut config = SettingsConfig::default();
        let mut order = SettingOrder::new();
        let mut seen = HashSet::new();
        for (name, value) in &shape.settings {
            if !seen.insert(name.as_str()) {
                return Err(ShapeError::field(
                    "http2.settings",
                    format!("duplicate setting: {name}"),
                ));
            }
            let id = match name.as_str() {
                "HEADER_TABLE_SIZE" => {
                    config.header_table_size = Some(*value);
                    SettingId::HeaderTableSize
                }
                "ENABLE_PUSH" if *value <= 1 => {
                    config.enable_push = Some(*value);
                    SettingId::EnablePush
                }
                "MAX_CONCURRENT_STREAMS" => {
                    config.max_concurrent_streams = Some(*value);
                    SettingId::MaxConcurrentStreams
                }
                "INITIAL_WINDOW_SIZE" if *value <= MAX_STREAM_ID => {
                    config.initial_window_size = Some(*value);
                    SettingId::InitialWindowSize
                }
                "MAX_FRAME_SIZE" if (16_384..=16_777_215).contains(value) => {
                    config.max_frame_size = Some(*value);
                    SettingId::MaxFrameSize
                }
                "MAX_HEADER_LIST_SIZE" => {
                    config.max_header_list_size = Some(*value);
                    SettingId::MaxHeaderListSize
                }
                "ENABLE_CONNECT_PROTOCOL" if *value <= 1 => {
                    config.enable_connect_protocol = Some(*value);
                    SettingId::EnableConnectProtocol
                }
                "NO_RFC7540_PRIORITIES" if *value <= 1 => {
                    config.no_rfc7540_priorities = Some(*value);
                    SettingId::NoRfc7540Priorities
                }
                "ENABLE_PUSH"
                | "INITIAL_WINDOW_SIZE"
                | "MAX_FRAME_SIZE"
                | "ENABLE_CONNECT_PROTOCOL"
                | "NO_RFC7540_PRIORITIES" => {
                    return Err(ShapeError::field(
                        "http2.settings",
                        format!("invalid value {value} for {name}"),
                    ));
                }
                _ => {
                    return Err(ShapeError::field(
                        "http2.settings",
                        format!("unknown setting: {name}"),
                    ));
                }
            };
            order.push(id);
        }
        config.setting_order = Some(order.clone());
        result.setting_config = config;
        result.setting_order = Some(order);
    }

    if !shape.pseudo_order.is_empty() {
        let mut order = PseudoHeaderOrder::new();
        let mut seen = HashSet::new();
        for name in &shape.pseudo_order {
            let header = match name.as_str() {
                "m" => PseudoHeader::Method,
                "s" => PseudoHeader::Scheme,
                "a" => PseudoHeader::Authority,
                "p" => PseudoHeader::Path,
                _ => {
                    return Err(ShapeError::field(
                        "http2.pseudo_order",
                        format!("unknown pseudo-header: {name}"),
                    ));
                }
            };
            if !seen.insert(header) {
                return Err(ShapeError::field(
                    "http2.pseudo_order",
                    format!("duplicate pseudo-header: {name}"),
                ));
            }
            order.push(header);
        }
        if seen.len() != 4 {
            return Err(ShapeError::field(
                "http2.pseudo_order",
                "must contain m, s, a, and p exactly once",
            ));
        }
        result.pseudo_header_order = Some(order);
    }

    if !shape.early_frames.is_empty() {
        let mut frames = Vec::with_capacity(shape.early_frames.len());
        for frame in &shape.early_frames {
            frames.push(match *frame {
                EarlyFrameShape::WindowUpdate { stream_id, increment } => {
                    if stream_id > MAX_STREAM_ID || !(1..=MAX_STREAM_ID).contains(&increment) {
                        return Err(ShapeError::field("http2.early_frames", "WINDOW_UPDATE requires a 31-bit stream ID and an increment in 1..=2^31-1"));
                    }
                    EarlyFrame::WindowUpdate(WindowUpdate::new(StreamId::from(stream_id), increment))
                }
                EarlyFrameShape::Priority { stream_id, dependency_id, weight, exclusive } => {
                    if stream_id == 0 || stream_id > MAX_STREAM_ID || dependency_id > MAX_STREAM_ID || stream_id == dependency_id {
                        return Err(ShapeError::field("http2.early_frames", "PRIORITY requires a nonzero 31-bit stream ID and a distinct 31-bit dependency ID"));
                    }
                    EarlyFrame::Priority(Priority::new(
                        StreamId::from(stream_id),
                        StreamDependency::new(StreamId::from(dependency_id), weight, exclusive),
                    ))
                }
            });
        }
        result.early_frames = Some(frames);
    }

    if !matches!(shape.content_length.as_str(), "auto" | "omit") {
        return Err(ShapeError::field(
            "http2.content_length",
            "must be auto or omit",
        ));
    }
    result.keep_alive_interval = shape.keepalive.interval_ms.map(Duration::from_millis);
    result.keep_alive_timeout = Duration::from_millis(shape.keepalive.timeout_ms);
    result.keep_alive_while_idle = shape.keepalive.while_idle;
    Ok(result)
}
