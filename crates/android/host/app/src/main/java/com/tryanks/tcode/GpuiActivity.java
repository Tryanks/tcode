package com.tryanks.tcode;

import android.app.NativeActivity;
import android.app.Activity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ActivityInfo;
import android.content.pm.PackageManager;
import android.graphics.Color;
import android.os.Build;
import android.os.Bundle;
import android.text.Editable;
import android.text.InputType;
import android.text.SpannableStringBuilder;
import android.view.Gravity;
import android.view.KeyEvent;
import android.view.View;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;
import android.widget.FrameLayout;

/** Minimal NativeActivity host for GPUI. */
public final class GpuiActivity extends NativeActivity {
    private static final int REQUEST_CAMERA = 6102;
    private static final int HOST_OK = 0;
    private static final int HOST_CANCELLED = 1;
    private static final int HOST_ERROR = 2;

    private GpuiInputView inputView;
    private long cameraRequest;

    private native void nativeCommitText(String text);
    private native void nativeSetComposingText(String text);
    private native void nativeFinishComposingText();
    private native void nativeDeleteBackward();
    private native void nativeKeyEvent(int keyCode, boolean down, int unicodeCodePoint, int metaState);
    private native void nativeOnInsets(int left, int top, int right, int bottom, int imeBottom);
    private native void nativeOnBack(boolean enabled);
    private native void nativeQrScanCompleted(long requestId, int status, String value);

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        ensureNativeLibraryVisibleToJvm();
        super.onCreate(savedInstanceState);
        configureEdgeToEdgeWindow();

        inputView = new GpuiInputView(this);
        inputView.setFocusable(false);
        inputView.setFocusableInTouchMode(false);
        inputView.setAlpha(0.01f);
        FrameLayout.LayoutParams layout = new FrameLayout.LayoutParams(1, 1);
        layout.gravity = Gravity.BOTTOM | Gravity.START;
        addContentView(inputView, layout);

        View decor = getWindow().getDecorView();
        decor.setOnApplyWindowInsetsListener((view, insets) -> {
            publishInsets(insets);
            return insets;
        });
        decor.requestApplyInsets();
    }

    @Override
    protected void onResume() {
        super.onResume();
        View decor = getWindow().getDecorView();
        decor.post(() -> {
            WindowInsets insets = decor.getRootWindowInsets();
            if (insets != null) publishInsets(insets);
        });
    }

    private void ensureNativeLibraryVisibleToJvm() {
        try {
            ActivityInfo info = getPackageManager().getActivityInfo(
                    new ComponentName(this, getClass()), PackageManager.GET_META_DATA);
            String library = info.metaData == null
                    ? null : info.metaData.getString("android.app.lib_name");
            if (library == null || library.isEmpty()) {
                throw new IllegalStateException("android.app.lib_name is required");
            }
            System.loadLibrary(library);
        } catch (PackageManager.NameNotFoundException error) {
            throw new IllegalStateException("Unable to resolve GPUI activity metadata", error);
        }
    }

    @SuppressWarnings("deprecation")
    private void configureEdgeToEdgeWindow() {
        Window window = getWindow();
        window.setStatusBarColor(Color.TRANSPARENT);
        window.setNavigationBarColor(Color.TRANSPARENT);
        if (Build.VERSION.SDK_INT >= 29) {
            window.setStatusBarContrastEnforced(false);
            window.setNavigationBarContrastEnforced(false);
        }
        if (Build.VERSION.SDK_INT >= 30) {
            window.setDecorFitsSystemWindows(false);
            WindowInsetsController controller = window.getInsetsController();
            if (controller != null) {
                int lightBars = WindowInsetsController.APPEARANCE_LIGHT_STATUS_BARS
                        | WindowInsetsController.APPEARANCE_LIGHT_NAVIGATION_BARS;
                controller.setSystemBarsAppearance(lightBars, lightBars);
            }
        } else {
            window.getDecorView().setSystemUiVisibility(
                    View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                            | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                            | View.SYSTEM_UI_FLAG_LIGHT_STATUS_BAR
                            | View.SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR);
        }
    }

    @SuppressWarnings("deprecation")
    private void publishInsets(WindowInsets insets) {
        if (Build.VERSION.SDK_INT >= 30) {
            android.graphics.Insets safe = insets.getInsets(
                    WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout());
            android.graphics.Insets ime = insets.getInsets(WindowInsets.Type.ime());
            nativeOnInsets(safe.left, safe.top, safe.right, safe.bottom, ime.bottom);
        } else {
            nativeOnInsets(insets.getStableInsetLeft(), insets.getStableInsetTop(),
                    insets.getStableInsetRight(), insets.getStableInsetBottom(),
                    insets.getSystemWindowInsetBottom());
        }
    }

    @Override
    @SuppressWarnings("deprecation")
    public void onBackPressed() { nativeOnBack(true); }

    public void gpuiShowKeyboard() {
        inputView.setFocusable(true);
        inputView.setFocusableInTouchMode(true);
        inputView.requestFocus();
        inputView.post(() -> ((InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE))
                .showSoftInput(inputView, InputMethodManager.SHOW_IMPLICIT));
    }

    public void gpuiHideKeyboard() {
        InputMethodManager manager =
                (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
        manager.hideSoftInputFromWindow(inputView.getWindowToken(), 0);
        inputView.clearFocus();
        inputView.setFocusable(false);
        inputView.setFocusableInTouchMode(false);
    }

    public void gpuiConfigureInput(
            boolean autocorrect, int autocapitalize, boolean suggestions, int inputAction) {
        inputView.configure(autocorrect, autocapitalize, suggestions, inputAction);
        ((InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE)).restartInput(inputView);
    }

    public void gpuiFinish() { finish(); }

    public String gpuiDataDir() {
        return getFilesDir().getAbsolutePath();
    }

    public String gpuiDeviceModel() {
        return Build.MODEL == null || Build.MODEL.isEmpty() ? "Android" : Build.MODEL;
    }

    public void gpuiStartCameraScan(long requestId) {
        if (cameraRequest != 0) {
            nativeQrScanCompleted(requestId, HOST_ERROR, "相机扫描正在进行");
            return;
        }
        cameraRequest = requestId;
        try {
            startActivityForResult(new Intent(this, QrScannerActivity.class), REQUEST_CAMERA);
        } catch (RuntimeException error) {
            cameraRequest = 0;
            nativeQrScanCompleted(requestId, HOST_ERROR, errorMessage(error));
        }
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode != REQUEST_CAMERA) {
            return;
        }
        long request = cameraRequest;
        cameraRequest = 0;
        String value = data == null ? null : data.getStringExtra(QrScannerActivity.EXTRA_VALUE);
        String error = data == null ? null : data.getStringExtra(QrScannerActivity.EXTRA_ERROR);
        if (resultCode == Activity.RESULT_OK && value != null) {
            nativeQrScanCompleted(request, HOST_OK, value);
        } else if (error != null) {
            nativeQrScanCompleted(request, HOST_ERROR, error);
        } else {
            nativeQrScanCompleted(request, HOST_CANCELLED, "已取消扫描");
        }
    }

    public String gpuiReadClipboard() {
        ClipboardManager clipboard =
                (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        ClipData clip = clipboard.getPrimaryClip();
        if (clip == null || clip.getItemCount() == 0) return null;
        CharSequence text = clip.getItemAt(0).coerceToText(this);
        return text == null ? null : text.toString();
    }

    public void gpuiWriteClipboard(String text) {
        ClipboardManager clipboard =
                (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        clipboard.setPrimaryClip(ClipData.newPlainText("tcode", text));
    }

    private static String errorMessage(Throwable error) {
        String message = error.getMessage();
        return message == null || message.isEmpty() ? error.getClass().getSimpleName() : message;
    }

    private final class GpuiInputView extends View {
        private int inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_MULTI_LINE;
        private int imeOptions = EditorInfo.IME_ACTION_NONE;
        private final Editable editable = new SpannableStringBuilder();

        GpuiInputView(Context context) {
            super(context);
            if (Build.VERSION.SDK_INT >= 33) setAutoHandwritingEnabled(false);
        }

        void configure(boolean autocorrect, int autocapitalize, boolean suggestions, int action) {
            int type = InputType.TYPE_CLASS_TEXT;
            if (autocorrect) type |= InputType.TYPE_TEXT_FLAG_AUTO_CORRECT;
            if (!suggestions) type |= InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS;
            if (autocapitalize == 1) type |= InputType.TYPE_TEXT_FLAG_CAP_WORDS;
            if (autocapitalize == 2) type |= InputType.TYPE_TEXT_FLAG_CAP_SENTENCES;
            if (autocapitalize == 3) type |= InputType.TYPE_TEXT_FLAG_CAP_CHARACTERS;
            inputType = type;
            switch (action) {
                case 2: imeOptions = EditorInfo.IME_ACTION_DONE; break;
                case 3: imeOptions = EditorInfo.IME_ACTION_GO; break;
                case 4: imeOptions = EditorInfo.IME_ACTION_NEXT; break;
                case 5: imeOptions = EditorInfo.IME_ACTION_PREVIOUS; break;
                case 6: imeOptions = EditorInfo.IME_ACTION_SEARCH; break;
                case 7: imeOptions = EditorInfo.IME_ACTION_SEND; break;
                default: imeOptions = EditorInfo.IME_ACTION_NONE;
            }
        }

        @Override public boolean onCheckIsTextEditor() { return true; }

        @Override
        public InputConnection onCreateInputConnection(EditorInfo outAttrs) {
            outAttrs.inputType = inputType;
            outAttrs.imeOptions = imeOptions | EditorInfo.IME_FLAG_NO_EXTRACT_UI;
            outAttrs.initialSelStart = editable.length();
            outAttrs.initialSelEnd = editable.length();
            return new BaseInputConnection(this, true) {
                @Override public Editable getEditable() { return editable; }
                @Override public boolean commitText(CharSequence text, int cursor) {
                    nativeCommitText(text == null ? "" : text.toString());
                    return super.commitText(text, cursor);
                }
                @Override public boolean setComposingText(CharSequence text, int cursor) {
                    nativeSetComposingText(text == null ? "" : text.toString());
                    return super.setComposingText(text, cursor);
                }
                @Override public boolean finishComposingText() {
                    nativeFinishComposingText(); return super.finishComposingText();
                }
                @Override public boolean deleteSurroundingText(int before, int after) {
                    if (before > 0) nativeDeleteBackward();
                    return super.deleteSurroundingText(before, after);
                }
                @Override public boolean sendKeyEvent(KeyEvent event) {
                    forwardKeyEvent(event); return true;
                }
                @Override public boolean performEditorAction(int actionCode) {
                    nativeKeyEvent(KeyEvent.KEYCODE_ENTER, true, '\n', 0);
                    nativeKeyEvent(KeyEvent.KEYCODE_ENTER, false, '\n', 0);
                    return true;
                }
            };
        }

        @Override public boolean onKeyDown(int keyCode, KeyEvent event) {
            forwardKeyEvent(event); return true;
        }
        @Override public boolean onKeyUp(int keyCode, KeyEvent event) {
            forwardKeyEvent(event); return true;
        }
        @Override public boolean onTouchEvent(android.view.MotionEvent event) { return false; }

        private void forwardKeyEvent(KeyEvent event) {
            nativeKeyEvent(event.getKeyCode(), event.getAction() == KeyEvent.ACTION_DOWN,
                    event.getUnicodeChar(), event.getMetaState());
        }
    }
}
