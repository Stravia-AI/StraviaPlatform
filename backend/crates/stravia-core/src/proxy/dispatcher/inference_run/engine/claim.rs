use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use axum::response::Response;
use futures::Stream;

struct SharedAdmission {
    remaining: AtomicU8,
    lease: Mutex<Option<crate::admission::PrincipalAdmissionLease>>,
}

pub(super) struct AdmissionHold {
    shared: Arc<SharedAdmission>,
    active: bool,
}

impl AdmissionHold {
    fn release(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        if self.shared.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared
                .lease
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
        }
    }
}

impl Drop for AdmissionHold {
    fn drop(&mut self) {
        self.release();
    }
}

pub(super) fn split_admission(
    lease: crate::admission::PrincipalAdmissionLease,
) -> (AdmissionHold, AdmissionHold) {
    let shared = Arc::new(SharedAdmission {
        remaining: AtomicU8::new(2),
        lease: Mutex::new(Some(lease)),
    });
    (
        AdmissionHold {
            shared: Arc::clone(&shared),
            active: true,
        },
        AdmissionHold {
            shared,
            active: true,
        },
    )
}

pub(super) struct DeliveryLeaseStream {
    inner: Pin<Box<dyn Stream<Item = Result<bytes::Bytes, axum::Error>> + Send>>,
    admission: Option<AdmissionHold>,
}

impl Stream for DeliveryLeaseStream {
    type Item = Result<bytes::Bytes, axum::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(context) {
            Poll::Ready(None) => {
                self.admission.take();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(error))) => {
                self.admission.take();
                Poll::Ready(Some(Err(error)))
            }
            other => other,
        }
    }
}

pub(super) fn wrap_delivery(response: Response, admission: AdmissionHold) -> Response {
    let (parts, body) = response.into_parts();
    let stream = DeliveryLeaseStream {
        inner: Box::pin(body.into_data_stream()),
        admission: Some(admission),
    };
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
}
