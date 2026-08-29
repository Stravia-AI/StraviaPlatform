use super::CallbackCopy;

pub(super) static COPY: CallbackCopy = CallbackCopy {
    lang: "en-US",
    complete_title: "OAuth complete",
    complete_message: "Authorization succeeded. Return to Stravia to save the provider.",
    failed_title: "OAuth could not be completed",
    failed_message: "Authorization failed. Return to Stravia for details and retry guidance.",
};
