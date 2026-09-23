use std::fmt;

macro_rules! identity {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl std::ops::Deref for $name {
            type Target = str;
            fn deref(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<String> for $name {
            fn eq(&self, other: &String) -> bool {
                self.0 == *other
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

identity!(
    /// Public identifier supplied by clients when selecting a Route.
    RouteId
);
identity!(
    /// Private persisted identifier of a Route aggregate.
    RouteKey
);
identity!(
    /// Persisted identifier of a Provider account.
    ProviderId
);
identity!(
    /// Model identifier supplied by the upstream Provider.
    UpstreamModelId
);
identity!(
    /// Private persisted identifier of one Route Target.
    TargetId
);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TargetDestination {
    ProviderOnly {
        provider_id: ProviderId,
    },
    Model {
        provider_id: ProviderId,
        model_id: UpstreamModelId,
    },
}

impl TargetDestination {
    pub fn new(provider_id: ProviderId, model: Option<UpstreamModelId>) -> Self {
        match model {
            Some(model_id) => Self::Model {
                provider_id,
                model_id,
            },
            None => Self::ProviderOnly { provider_id },
        }
    }

    pub fn provider_id(&self) -> &ProviderId {
        match self {
            Self::ProviderOnly { provider_id } | Self::Model { provider_id, .. } => provider_id,
        }
    }

    pub fn model(&self) -> Option<&UpstreamModelId> {
        match self {
            Self::ProviderOnly { .. } => None,
            Self::Model { model_id, .. } => Some(model_id),
        }
    }

    pub fn into_parts(self) -> (ProviderId, Option<UpstreamModelId>) {
        match self {
            Self::ProviderOnly { provider_id } => (provider_id, None),
            Self::Model {
                provider_id,
                model_id,
            } => (provider_id, Some(model_id)),
        }
    }
}
