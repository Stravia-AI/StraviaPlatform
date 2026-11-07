//! Request body with optional HTTP/2 content-length suppression.

use std::{
    convert::Infallible,
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use rama_http_types::body::{Frame, SizeHint, StreamingBody, util::Full};

/// A buffered request body whose size can be hidden from the serializer.
#[derive(Debug)]
pub struct RequestBody {
    inner: Full<Bytes>,
    omit_length: bool,
    // Rama must poll an empty explicitly chunked body to emit its final chunk.
    pending_empty_chunk: bool,
}

impl RequestBody {
    pub fn new(bytes: Bytes, omit_length: bool) -> Self {
        Self {
            inner: Full::new(bytes),
            omit_length,
            pending_empty_chunk: false,
        }
    }

    pub(super) fn require_empty_chunk(&mut self) {
        self.pending_empty_chunk = self.inner.is_end_stream();
    }

    pub(super) fn actual_size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }

    pub(super) fn omit_length(&self) -> bool {
        self.omit_length
    }
}

impl StreamingBody for RequestBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.pending_empty_chunk = false;
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        !self.pending_empty_chunk && self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        if self.omit_length {
            SizeHint::new()
        } else {
            self.inner.size_hint()
        }
    }
}
