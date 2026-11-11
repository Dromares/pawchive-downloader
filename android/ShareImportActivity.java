package pw.pawchive.downloader;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;

/**
 * Share target for Pawchive Downloader.
 *
 * android.app.NativeActivity cannot receive Intents: with no Java class of our own there is
 * nobody to override onNewIntent()/setIntent(), and the framework NativeActivity drops them.
 * This 20-line shim catches the incoming SEND/VIEW Intent, pulls out the shared link, and
 * relaunches the native activity (framework class — no metaActivity indirection) with the link
 * carried in EXTRA_TEXT, where the Rust side reads it via getIntent().
 */
public class ShareImportActivity extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Intent in = getIntent();
        String text = in.getStringExtra(Intent.EXTRA_TEXT);
        if (text == null || text.isEmpty()) {
            android.net.Uri data = in.getData();
            text = data != null ? data.toString() : null;
        }
        if (text != null && !text.isEmpty()) {
            Intent launch = new Intent("android.intent.action.VIEW");
            launch.setClassName(getPackageName(), "android.app.NativeActivity");
            launch.putExtra(Intent.EXTRA_TEXT, text);
            launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_CLEAR_TASK);
            startActivity(launch);
        }
        finish();
    }
}
