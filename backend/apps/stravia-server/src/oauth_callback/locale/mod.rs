mod en_us;
mod zh_cn;

#[derive(Clone, Copy)]
pub(super) enum CallbackLocale {
    EnUs,
    ZhCn,
}

pub(super) struct CallbackCopy {
    pub lang: &'static str,
    pub complete_title: &'static str,
    pub complete_message: &'static str,
    pub failed_title: &'static str,
    pub failed_message: &'static str,
}

impl CallbackLocale {
    pub fn from_requested(locale: Option<&str>) -> Self {
        match locale {
            Some("zh-CN") => Self::ZhCn,
            _ => Self::EnUs,
        }
    }

    pub fn copy(self) -> &'static CallbackCopy {
        match self {
            Self::EnUs => &en_us::COPY,
            Self::ZhCn => &zh_cn::COPY,
        }
    }
}
