//! Wire-level regression coverage for configurable HTTP/2 client handshakes.

use std::{io::Cursor, ops::ControlFlow, time::Duration};

use bytes::{Bytes, BytesMut};
use rama_core::svc_input::ServiceInput;
use rama_http_types::{
    Request,
    proto::h2::{
        PseudoHeader, PseudoHeaderOrder,
        frame::{SettingId, SettingOrder, SettingsConfig},
        hpack::Decoder,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

use super::{
    body::RequestBody,
    h2::{H2Settings, connect},
};

struct Frame {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: Vec<u8>,
}

async fn read_frame(stream: &mut TcpStream) -> Frame {
    let mut header = [0; 9];
    stream.read_exact(&mut header).await.unwrap();
    let len =
        (usize::from(header[0]) << 16) | (usize::from(header[1]) << 8) | usize::from(header[2]);
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).await.unwrap();
    Frame {
        kind: header[3],
        flags: header[4],
        stream: u32::from_be_bytes(header[5..9].try_into().unwrap()) & 0x7fff_ffff,
        payload,
    }
}

#[tokio::test]
async fn configured_settings_and_pseudo_headers_reach_the_wire() {
    timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut preface = [0; 24];
            stream.read_exact(&mut preface).await.unwrap();
            assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
            // An empty server SETTINGS frame allows the client to start streams.
            stream
                .write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])
                .await
                .unwrap();

            let mut settings = None;
            let mut window = None;
            let mut headers = BytesMut::new();
            let mut headers_complete = false;
            let mut continuing = false;
            while settings.is_none() || window.is_none() || !headers_complete {
                let frame = read_frame(&mut stream).await;
                if continuing {
                    assert_eq!(frame.kind, 9, "expected CONTINUATION");
                    assert_eq!(frame.stream, 1);
                    headers.extend_from_slice(&frame.payload);
                    continuing = frame.flags & 4 == 0;
                    headers_complete = !continuing;
                    continue;
                }
                match frame.kind {
                    4 if frame.flags & 1 == 0 => {
                        assert_eq!(frame.stream, 0);
                        assert!(settings.replace(frame.payload).is_none());
                    }
                    8 if frame.stream == 0 => {
                        assert_eq!(frame.payload.len(), 4);
                        assert!(
                            window
                                .replace(
                                    u32::from_be_bytes(frame.payload.try_into().unwrap())
                                        & 0x7fff_ffff
                                )
                                .is_none()
                        );
                    }
                    1 => {
                        assert_eq!(frame.stream, 1);
                        // This request uses neither padding nor priority.
                        assert_eq!(frame.flags & 0x28, 0);
                        headers.extend_from_slice(&frame.payload);
                        continuing = frame.flags & 4 == 0;
                        headers_complete = !continuing;
                    }
                    _ => {}
                }
            }
            (settings.unwrap(), window.unwrap(), headers)
        });

        let setting_order: SettingOrder = [
            SettingId::MaxHeaderListSize,
            SettingId::InitialWindowSize,
            SettingId::HeaderTableSize,
            SettingId::MaxFrameSize,
            SettingId::EnablePush,
            SettingId::MaxConcurrentStreams,
        ]
        .into_iter()
        .collect();
        let pseudo_header_order: PseudoHeaderOrder = [
            PseudoHeader::Path,
            PseudoHeader::Authority,
            PseudoHeader::Method,
            PseudoHeader::Scheme,
        ]
        .into_iter()
        .collect();
        let settings = H2Settings {
            setting_config: SettingsConfig {
                header_table_size: Some(8192),
                enable_push: Some(0),
                max_concurrent_streams: Some(123),
                initial_window_size: Some(100_001),
                max_frame_size: Some(32_768),
                max_header_list_size: Some(54_321),
                ..Default::default()
            },
            setting_order: Some(setting_order),
            pseudo_header_order: Some(pseudo_header_order),
            initial_stream_window_size: Some(100_002),
            initial_connection_window_size: Some(200_000),
            keep_alive_interval: None,
            ..Default::default()
        };
        let socket = TcpStream::connect(address).await.unwrap();
        let (mut sender, _peer_settings) =
            connect(ServiceInput::new(socket), settings).await.unwrap();
        sender.ready().await.unwrap();
        let request = Request::builder()
            .uri("http://example.test/wire?x=1")
            .body(RequestBody::new(Bytes::new(), false))
            .unwrap();
        let response = tokio::spawn(async move { sender.send_request(request).await });
        let (settings, window, mut headers) = server.await.unwrap();
        response.abort();

        assert_eq!(settings.len() % 6, 0);
        let entries: Vec<(u16, u32)> = settings
            .as_chunks::<6>()
            .0
            .iter()
            .map(|entry| {
                (
                    u16::from_be_bytes(entry[..2].try_into().unwrap()),
                    u32::from_be_bytes(entry[2..].try_into().unwrap()),
                )
            })
            .collect();
        assert_eq!(
            entries,
            vec![
                (6, 54_321),
                (4, 100_002),
                (1, 8192),
                (5, 32_768),
                (2, 0),
                (3, 123),
            ]
        );
        assert_eq!(window, 200_000 - 65_535);

        let mut names = Vec::new();
        Decoder::new(4096)
            .decode(&mut Cursor::new(&mut headers), |header| {
                names.push(header.name().as_slice().to_vec());
                ControlFlow::Continue(())
            })
            .unwrap();
        let pseudo_names: Vec<&[u8]> = names
            .iter()
            .map(Vec::as_slice)
            .filter(|name| name.starts_with(b":"))
            .collect();
        assert_eq!(
            pseudo_names,
            vec![
                b":path".as_slice(),
                b":authority".as_slice(),
                b":method".as_slice(),
                b":scheme".as_slice(),
            ]
        );
    })
    .await
    .expect("HTTP/2 wire capture timed out");
}
