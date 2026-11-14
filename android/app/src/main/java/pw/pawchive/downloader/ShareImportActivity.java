package pw.pawchive.downloader;

import android.app.Activity;
import android.content.Intent;
import android.net.Uri;
import android.os.Bundle;
import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;

/**
 * Share target for Pawchive Downloader (browser "Share", or a tapped pawchive.pw link).
 *
 * The app itself is a Rust android.app.NativeActivity, which cannot receive new Intents, and it
 * must never be relaunched while it is running: a second NativeActivity in the same process
 * starts a second winit EventLoop, winit refuses ("EventLoop can't be recreated"), and the whole
 * process exits, taking any running download with it.
 *
 * So this shim never touches the running activity's lifecycle. It writes the shared text to
 * {@code <filesDir>/shared_link} (atomically: temp file + rename), then brings the app forward
 * with the launcher Intent, which resumes the existing task instead of creating a new activity
 * (the native activity is also singleTask as a backstop). The Rust side consumes the file
 * (crates/pawchive-gui/src/paste.rs, take_shared_link).
 */
public class ShareImportActivity extends Activity {
    /** Keep in sync with HANDOFF in crates/pawchive-gui/src/paste.rs. */
    static final String HANDOFF = "shared_link";

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Intent in = getIntent();
        String text = in.getStringExtra(Intent.EXTRA_TEXT);
        if (text == null || text.trim().isEmpty()) {
            Uri data = in.getData();
            text = data != null ? data.toString() : null;
        }
        if (text != null && !text.trim().isEmpty()) {
            File dir = getFilesDir();
            File tmp = new File(dir, HANDOFF + ".tmp");
            boolean written = false;
            try (FileOutputStream out = new FileOutputStream(tmp)) {
                out.write(text.trim().getBytes(StandardCharsets.UTF_8));
                written = true;
            } catch (Exception e) {
                written = false;
            }
            if (!written || !tmp.renameTo(new File(dir, HANDOFF))) {
                tmp.delete();
            }
        }
        Intent launch = getPackageManager().getLaunchIntentForPackage(getPackageName());
        if (launch != null) {
            launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_RESET_TASK_IF_NEEDED);
            startActivity(launch);
        }
        finish();
    }
}
