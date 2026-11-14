//! Text entry on Android, for a NativeActivity app the soft keyboard cannot reach (winit 0.30's
//! Android backend implements no InputConnection, so keyboard commits never arrive). Two doors:
//!
//! * **Share / tapped link**: `ShareImportActivity`, the APK's small Java shim, writes the shared
//!   text to `<filesDir>/shared_link` and brings the already-running app forward;
//!   [`take_shared_link`] consumes it. A file, not an Intent, on purpose: NativeActivity never
//!   sees new Intents, and relaunching it to deliver one starts a second winit `EventLoop` in the
//!   same process, which winit refuses ("EventLoop can't be recreated"); the process then exits
//!   and takes any running download with it.
//! * **Paste button**: [`clipboard_text`] reads `ClipboardManager` over JNI (the Android twin of
//!   DAN-1600's desktop paste). Android may refuse the read under its focus rules, and a refused
//!   read looks exactly like an empty clipboard.

/// The file the Java share shim writes. Keep in sync with `ShareImportActivity.HANDOFF`.
#[cfg(target_os = "android")]
const HANDOFF: &str = "shared_link";

/// `<filesDir>/shared_link`: android-activity's internal data path IS the Java `getFilesDir()`.
#[cfg(target_os = "android")]
fn handoff_path() -> Option<std::path::PathBuf> {
    Some(crate::android_app()?.internal_data_path()?.join(HANDOFF))
}

/// A shared link is waiting (it arrived mid-scan/download and must not clobber the run).
#[cfg(target_os = "android")]
pub fn shared_link_waiting() -> bool {
    handoff_path().is_some_and(|p| p.exists())
}

/// The shared link, consumed (read, then deleted). `None` when nothing is waiting.
#[cfg(target_os = "android")]
pub fn take_shared_link() -> Option<String> {
    let path = handoff_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    link_from_shared_text(&raw)
}

/// Browsers share "Page title https://…": keep the first http(s) link, minus trailing punctuation.
/// Text with no link in it comes back trimmed, so the link field's own check can explain why it
/// isn't a Pawchive link instead of the share silently doing nothing.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) fn link_from_shared_text(raw: &str) -> Option<String> {
    let start = [raw.find("https://"), raw.find("http://")].into_iter().flatten().min();
    let link = match start {
        Some(i) => {
            let tail = &raw[i..];
            let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
            tail[..end]
                .trim_end_matches(|c: char| {
                    matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '>' | '"' | '\'')
                })
                .to_owned()
        }
        None => raw.trim().to_owned(),
    };
    (!link.is_empty()).then_some(link)
}

/// The clipboard's text: `None` when it is empty, Android refused the read, or the JNI walk failed.
#[cfg(target_os = "android")]
pub fn clipboard_text() -> Option<String> {
    // SAFETY: ndk_context holds the process's JavaVM and Application object for the whole process
    // lifetime, and this runs on winit's main thread, which is attached to the JVM.
    unsafe {
        let rt = ndk_context::android_context();
        let vm = jni::JavaVM::from_raw(rt.vm() as *mut jni::sys::JavaVM).ok()?;
        // As daemon: the guard must NOT detach on drop, because this thread belongs to winit.
        let mut env = vm.attach_current_thread_as_daemon().ok()?;
        let app_ctx = rt.context() as jni::sys::jobject; // the Application object: a Context
                                                         // This thread never returns to Java, so a plain local ref would live forever (jni 0.21's
                                                         // JObject frees nothing on drop). A local frame frees everything made inside it.
        let text = env
            .with_local_frame(16, |env| -> jni::errors::Result<Option<String>> { Ok(read_clipboard(env, app_ctx)) })
            .ok()
            .flatten();
        // Never leave a Java exception pending on winit's thread: the next JNI call, ours or
        // android-activity's, would abort the whole process.
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
        text
    }
}

/// getSystemService("clipboard") -> getPrimaryClip() -> getItemAt(0) -> Item.coerceToText(ctx).
/// Every signature here uses `java/lang/CharSequence`; an `android/text/...` typo throws
/// NoSuchMethodError, which is exactly how the first version of this walk silently returned None.
#[cfg(target_os = "android")]
unsafe fn read_clipboard(env: &mut jni::JNIEnv<'_>, app_ctx: jni::sys::jobject) -> Option<String> {
    use jni::objects::{JObject, JString, JValue};

    let ctx = JObject::from_raw(app_ctx);
    let service = env.new_string("clipboard").ok()?;
    let manager = env
        .call_method(&ctx, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[JValue::Object(&service)])
        .ok()?
        .l()
        .ok()?;
    if manager.is_null() {
        return None;
    }
    let clip = env.call_method(&manager, "getPrimaryClip", "()Landroid/content/ClipData;", &[]).ok()?.l().ok()?;
    if clip.is_null() {
        return None; // empty, or Android refused the read
    }
    let item =
        env.call_method(&clip, "getItemAt", "(I)Landroid/content/ClipData$Item;", &[JValue::Int(0)]).ok()?.l().ok()?;
    if item.is_null() {
        return None;
    }
    // coerceToText lives on ClipData.Item (not ClipData) and turns URI/Intent clips into text too.
    let chars = env
        .call_method(
            &item,
            "coerceToText",
            "(Landroid/content/Context;)Ljava/lang/CharSequence;",
            &[JValue::Object(&ctx)],
        )
        .ok()?
        .l()
        .ok()?;
    if chars.is_null() {
        return None;
    }
    let string = JString::from(env.call_method(&chars, "toString", "()Ljava/lang/String;", &[]).ok()?.l().ok()?);
    let text: String = env.get_string(&string).ok()?.into();
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::link_from_shared_text as pick;

    #[test]
    fn keeps_only_the_link_from_a_browser_share() {
        let shared = "ElysianRagazza | Pawchive https://pawchive.pw/patreon/user/163211671";
        assert_eq!(pick(shared).as_deref(), Some("https://pawchive.pw/patreon/user/163211671"));
        assert_eq!(
            pick("https://pawchive.pw/fanbox/user/1386751\n").as_deref(),
            Some("https://pawchive.pw/fanbox/user/1386751")
        );
        assert_eq!(
            pick("look (https://pawchive.pw/patreon/user/5).").as_deref(),
            Some("https://pawchive.pw/patreon/user/5")
        );
        assert_eq!(
            pick("http://pawchive.pw/patreon/user/5?o=50 more").as_deref(),
            Some("http://pawchive.pw/patreon/user/5?o=50")
        );
    }

    #[test]
    fn the_earliest_link_wins() {
        let two = "first http://pawchive.pw/patreon/user/1 then https://pawchive.pw/patreon/user/2";
        assert_eq!(pick(two).as_deref(), Some("http://pawchive.pw/patreon/user/1"));
    }

    #[test]
    fn text_without_a_link_passes_through_trimmed() {
        assert_eq!(pick("  just words  ").as_deref(), Some("just words"));
        assert_eq!(pick(" \n "), None);
    }
}
