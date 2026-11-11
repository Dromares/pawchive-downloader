//! Reading the Android system clipboard from a no-Java NativeActivity app (DAN-1600's Android twin).
//!
//! Desktop paste rides egui-winit's `clipboard` feature (arboard); neither arboard nor the system
//! "Paste" menu exists here, so the link field had no paste route at all on a phone. This calls
//! `ClipboardManager.getPrimaryClip()` through JNI against the activity handle that `android-activity`
//! already stashed (`ndk_context`), exactly like the TLS fix reached around the missing Java layer.
//!
//! Reading needs no permission: Android 10+ only restricts background reads, and this runs in the
//! focused activity, from the UI thread, on an explicit button press.

/// The current clipboard text, `None` when the clipboard is empty, holds a non-text clip, or the
/// JNI walk fails. A non-empty single-line text clip becomes the returned `String` as-is.
#[cfg(target_os = "android")]
pub fn clipboard_text() -> Option<String> {
    use jni::objects::{JObject, JValue};

    // SAFETY: `java_vm`/`android_context` hand back the pointers the activity registered at startup;
    // they are live for the whole process, and this runs on the activity's own thread.
    unsafe {
        let rt = ndk_context::android_context();
        let vm = jni::JavaVM::from_raw(rt.vm() as *mut jni::sys::JavaVM).ok()?;
        // As daemon: the guard must NOT detach on drop — this thread belongs to winit, not to us.
        let mut env = vm.attach_current_thread_as_daemon().ok()?;
        let activity = JObject::from_raw(rt.context() as jni::sys::jobject);

        let service = env.new_string("clipboard").ok()?;
        let manager = env
            .call_method(
                &activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[(&service).into()],
            )
            .ok()?
            .l()
            .ok()?;
        if manager.is_null() {
            return None;
        }
        let clip = match env.call_method(&manager, "getPrimaryClip", "()Landroid/content/ClipData;", &[]) {
            Ok(v) => v.l().map_err(|e| dbgline(&format!("RD clipconv {e}"))).ok()?,
            Err(e) => {
                dbgline(&format!("RD getPrimaryClip ERR {e}"));
                return None;
            }
        };
        if clip.is_null() {
            dbgline("RD clip_null");
            return None; // empty clipboard
        }
        // Walk ClipData properly: getItemAt(0) -> Item.getText()/coerceToText(Context).
        // Method IDs resolved explicitly so a bad signature shows up as a logged
        // NoSuchMethodError instead of vanishing into `?` (every failed JNI call leaves a
        // PENDING Java exception that poisons the whole thread until drained).
        let jargs = [jni::sys::jvalue { i: 0 }];
        let item = match env.get_method_id("android/content/ClipData", "getItemAt", "(I)Landroid/content/ClipData$Item;")
            .and_then(|mid| env.call_method_unchecked(&clip, mid, jni::signature::ReturnType::Object, &jargs))
        {
            Ok(v) => {
                if let Some(Some(msg)) = drain_exception(&mut env) {
                    dbgline(&format!("RD getItemAt EXC {msg}"));
                    return None;
                }
                match v.l() {
                    Ok(o) => o,
                    Err(e) => {
                        dbgline(&format!("RD itemconv ERR {e}"));
                        return None;
                    }
                }
            }
            Err(e) => {
                dbgline(&format!("RD getItemAt ERR {e}"));
                if let Some(Some(msg)) = drain_exception(&mut env) {
                    dbgline(&format!("RD getItemAt EXC {msg}"));
                }
                return None;
            }
        };
        if item.is_null() {
            dbgline("RD item_null");
            return None;
        }
        let app_ctx = JObject::from_raw(rt.context() as jni::sys::jobject); // Application IS a Context
        let cs = match env.get_method_id("android/content/ClipData$Item", "coerceToText", "(Landroid/content/Context;)Ljava/lang/CharSequence;")
            .and_then(|mid| {
                let jargs = [jni::sys::jvalue { l: app_ctx.as_raw() }];
                env.call_method_unchecked(&item, mid, jni::signature::ReturnType::Object, &jargs)
            })
        {
            Ok(v) => {
                if let Some(Some(msg)) = drain_exception(&mut env) {
                    dbgline(&format!("RD item.coerce EXC {msg}"));
                    return None;
                }
                match v.l() {
                    Ok(o) => o,
                    Err(e) => {
                        dbgline(&format!("RD csconv ERR {e}"));
                        return None;
                    }
                }
            }
            Err(e) => {
                dbgline(&format!("RD item.coerce ERR {e}"));
                if let Some(Some(msg)) = drain_exception(&mut env) {
                    dbgline(&format!("RD item.coerce EXC {msg}"));
                }
                return None;
            }
        };
        if cs.is_null() {
            dbgline("RD cs_null");
            return None;
        }
        let string = env.call_method(&cs, "toString", "()Ljava/lang/String;", &[]).ok()?.l().ok()?;
        let jstr = jni::objects::JString::from_raw(string.into_raw()); // must outlive the borrow below
        let text: String = env.get_string(&jstr).ok()?.into(); // JavaString: copies the UTF-16 into Rust
        dbgline(&format!("RD ok len={}", text.chars().count()));
        (!text.is_empty()).then_some(text)
    }
}

/// TEST-ONLY (and harmless forever): places `text` on the system clipboard via
/// `ClipboardManager.setPrimaryClip`. The emulator image has no scriptable way to fill the
/// clipboard, so the read path above is verified by writing through this same JNI surface.
#[cfg(target_os = "android")]
/// Test hook for driving clipboard scenarios from adb; never called in the shipped flow.
#[allow(dead_code)]
pub fn set_clipboard_text(text: &str) -> Option<()> {
    use jni::objects::{JObject, JValue};

    unsafe {
        let rt = ndk_context::android_context();
        let vm = jni::JavaVM::from_raw(rt.vm() as *mut jni::sys::JavaVM).ok()?;
        let mut env = vm.attach_current_thread_as_daemon().ok()?;
        let activity = JObject::from_raw(rt.context() as jni::sys::jobject);

        let service = env.new_string("clipboard").ok()?;
        let manager = env
            .call_method(
                &activity,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[(&service).into()],
            )
            .ok()?
            .l()
            .ok()?;
        if manager.is_null() {
            return None;
        }
        let label = env.new_string("pawchive").ok()?;
        let chars = env.new_string(text).ok()?;
        let clip = env
            .call_static_method(
                "android/content/ClipData",
                "newPlainText",
                "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;",
                &[(&label).into(), (&chars).into()],
            )
            .ok()?
            .l()
            .ok()?;
        let set = env.call_method(&manager, "setPrimaryClip", "(Landroid/content/ClipData;)V", &[JValue::Object(&clip).into()]);
        if let Some(Some(msg)) = drain_exception(&mut env) {
            dbgline(&format!("SET exc {msg}"));
        }
        set.is_ok().then_some(())
    }
}

/// The link entry point on Android — soft keyboards cannot deliver text to this app (winit's
/// Android backend has no InputConnection), so links ride Intents: browser "Share → Pawchive
/// Downloader" (ACTION_SEND text/plain) or a tapped link (ACTION_VIEW). Returns the URL out of
/// EXTRA_TEXT (Chrome sometimes prefixes the page title, so the first http(s) token wins) or
/// getDataString().

#[cfg(target_os = "android")]
pub fn intent_url() -> Option<String> {
    use jni::objects::JObject;

    let jstring = |env: &mut jni::JNIEnv<'_>, obj: jni::objects::JObject<'_>| -> Option<String> {
        let jstr = unsafe { jni::objects::JString::from_raw(obj.into_raw()) }; // must outlive the borrow
        env.get_string(&jstr).ok().map(String::from)
    };
    let app = crate::android_app()?;
    // IMPORTANT (crashed android.8 first attempt): `ndk_context` registers the APPLICATION object,
    // which has no `getIntent()` — the Intent lives on the ACTIVITY, reachable through
    // android-activity's `activity_as_ptr()`. Clipboard calls stay on the Application object
    // (getSystemService is a Context method, so both work there).
    unsafe {
        let vm = jni::JavaVM::from_raw(app.vm_as_ptr() as *mut jni::sys::JavaVM).ok()?;
        let mut env = vm.attach_current_thread_as_daemon().ok()?;
        let activity = JObject::from_raw(app.activity_as_ptr() as jni::sys::jobject);
        let intent = match env.call_method(&activity, "getIntent", "()Landroid/content/Intent;", &[]) {
            Ok(v) => v.l().ok()?,
            Err(_) => return None,
        };
        if intent.is_null() {
            return None;
        }
        let key = env.new_string("android.intent.extra.TEXT").ok()?;
        let extra = env
            .call_method(&intent, "getStringExtra", "(Ljava/lang/String;)Ljava/lang/String;", &[(&key).into()])
            .ok()?
            .l()
            .ok()?;
        let shared = (!extra.is_null()).then(|| jstring(&mut env, extra)).flatten();
        let data = env.call_method(&intent, "getDataString", "()Ljava/lang/String;", &[]).ok()?.l().ok()?;
        let viewed = (!data.is_null()).then(|| jstring(&mut env, data)).flatten();
        let raw = shared.or(viewed)?;
        let url = raw
            .split_whitespace()
            .find(|t| t.starts_with("http"))
            .map(str::to_owned)
            .unwrap_or_else(|| raw.trim().to_owned());
        (!url.is_empty()).then_some(url)
    }
}

/// Same intent only surfaces once, so clearing the field does not fight a zombie prefill.
#[cfg(target_os = "android")]
static LAST_IMPORT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg(target_os = "android")]
pub fn fresh_intent_url() -> Option<String> {
    let url = intent_url()?;
    let mut last = LAST_IMPORT.lock().unwrap();
    (last.as_deref() != Some(url.as_str())).then(|| {
        *last = Some(url.clone());
        url
    })
}

/// Pull any PENDING Java exception off the thread and render it — mandatory hygiene here:
/// a failed JNI call leaves the exception pending, and every later JNI call on this thread
/// (including intent reads!) then fails with "unexpected pending exception" until drained.
#[cfg(target_os = "android")]
unsafe fn drain_exception(env: &mut jni::JNIEnv<'_>) -> Option<Option<String>> {
    let t = env.exception_occurred().ok()?;
    if t.is_null() {
        return Some(None);
    }
    env.exception_clear().ok()?;
    let s = env.call_method(&t, "toString", "()Ljava/lang/String;", &[]).ok()?.l().ok()?;
    let jstr = jni::objects::JString::from_raw(s.into_raw());
    let owned: String = env.get_string(&jstr).ok()?.into();
    Some(Some(owned))
}

/// Failure breadcrumbs for the clipboard path; swallowed on purpose (UI shows one honest hint).
#[cfg(target_os = "android")]
fn dbgline(_s: &str) {}

