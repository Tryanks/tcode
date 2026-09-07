package com.tryanks.tcode;

import android.app.Activity;
import android.app.Application;
import android.os.Bundle;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.net.http.SslError;
import android.view.View;
import android.view.ViewGroup;
import android.view.KeyEvent;
import android.view.inputmethod.InputMethodManager;
import android.content.Context;
import android.webkit.*;
import android.widget.FrameLayout;
import java.io.ByteArrayOutputStream;
import java.util.HashMap;

/** Activity-owned children. Every entry point and WebView callback runs on the Java UI thread. */
public final class PreviewHost implements Application.ActivityLifecycleCallbacks {
    private final Activity activity;
    private final FrameLayout container;
    private final HashMap<Long, WebView> views = new HashMap<>();
    private boolean resumed = true;

    private static native void nativeResult(long request, String value, byte[] png, String error);
    private static native void nativeEvent(long view, int kind, String url, String title,
                                           int code, String message);

    public PreviewHost(Activity activity) {
        this.activity = activity;
        container = new FrameLayout(activity);
        activity.addContentView(container, new FrameLayout.LayoutParams(-1, -1));
        activity.getApplication().registerActivityLifecycleCallbacks(this);
    }

    private void event(long id, int kind, String url, int code, String message) {
        WebView view = views.get(id);
        if (view != null) nativeEvent(id, kind, url == null ? "" : url,
                view.getTitle() == null ? "" : view.getTitle(), code, message);
    }

    /** A fixed JNI signature keeps owned Rust arguments independent of local JNI references. */
    public void command(long id, long request, String operation, String value,
                        int x, int y, int width, int height) {
        try {
            if (operation.equals("create")) {
                if (views.containsKey(id)) throw new IllegalStateException("duplicate preview");
                WebView view = new WebView(activity) {
                    // NativeActivity otherwise sends unhandled keys to its GPUI input queue.
                    @Override public boolean dispatchKeyEvent(KeyEvent event) {
                        if (super.dispatchKeyEvent(event)) return true;
                        return event.getKeyCode() != KeyEvent.KEYCODE_BACK;
                    }
                };
                view.getSettings().setJavaScriptEnabled(true);
                view.getSettings().setDomStorageEnabled(true);
                view.getSettings().setMixedContentMode(WebSettings.MIXED_CONTENT_NEVER_ALLOW);
                view.getSettings().setAllowFileAccess(false);
                view.setVisibility(View.GONE);
                view.setWebViewClient(new WebViewClient() {
                    @Override public void onPageStarted(WebView v, String url, Bitmap icon) {
                        event(id, 0, url, 0, "");
                    }
                    @Override public void onPageFinished(WebView v, String url) {
                        event(id, 1, url, 0, "");
                    }
                    @Override public void doUpdateVisitedHistory(WebView v, String url, boolean reload) {
                        event(id, 2, url, 0, "");
                    }
                    @Override public void onReceivedError(WebView v, WebResourceRequest r, WebResourceError e) {
                        if (r.isForMainFrame()) event(id, 3, r.getUrl().toString(),
                                e.getErrorCode(), e.getDescription().toString());
                    }
                    @Override public void onReceivedHttpError(WebView v, WebResourceRequest r, WebResourceResponse e) {
                        if (r.isForMainFrame()) event(id, 4, r.getUrl().toString(),
                                e.getStatusCode(), e.getReasonPhrase());
                    }
                    @Override public void onReceivedSslError(WebView v, SslErrorHandler handler, SslError e) {
                        handler.cancel();
                        event(id, 5, e.getUrl(), e.getPrimaryError(), "TLS certificate validation failed");
                    }
                });
                view.setWebChromeClient(new WebChromeClient() {
                    @Override public void onReceivedTitle(WebView v, String title) {
                        event(id, 2, v.getUrl(), 0, "");
                    }
                });
                views.put(id, view);
                container.addView(view, new FrameLayout.LayoutParams(0, 0));
                event(id, 7, "about:blank", 0, "");
                view.loadUrl("about:blank");
                return;
            }
            WebView view = views.get(id);
            if (view == null) throw new IllegalStateException("preview browser is not open");
            switch (operation) {
                case "destroy":
                    blur(view);
                    views.remove(id);
                    container.removeView(view);
                    view.stopLoading();
                    view.destroy();
                    break;
                case "bounds":
                    FrameLayout.LayoutParams bounds = new FrameLayout.LayoutParams(width, height);
                    // FrameLayout lays out in its own window position, including any OS offset.
                    int[] origin = new int[2];
                    container.getLocationInWindow(origin);
                    bounds.leftMargin = x - origin[0];
                    bounds.topMargin = y - origin[1];
                    view.setLayoutParams(bounds);
                    break;
                case "show": view.setVisibility(View.VISIBLE); break;
                case "hide": blur(view); view.setVisibility(View.GONE); break;
                case "blur": blur(view); break;
                case "navigate": view.loadUrl(value); break;
                case "back": if (view.canGoBack()) view.goBack(); break;
                case "forward": if (view.canGoForward()) view.goForward(); break;
                case "reload": view.reload(); break;
                case "canGoBack": nativeResult(request, Boolean.toString(view.canGoBack()), null, null); break;
                case "evaluate":
                    view.evaluateJavascript(value, result -> nativeResult(request, result, null, null));
                    break;
                case "screenshot":
                    if (!resumed || !view.isShown() || view.getWidth() <= 0 || view.getHeight() <= 0)
                        throw new IllegalStateException("preview browser has no visible area");
                    if ((long)view.getWidth() * view.getHeight() > 16000000)
                        throw new IllegalStateException("preview screenshot exceeds 16 megapixels");
                    Bitmap bitmap = Bitmap.createBitmap(view.getWidth(), view.getHeight(), Bitmap.Config.ARGB_8888);
                    try {
                        view.draw(new Canvas(bitmap));
                        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
                        if (!bitmap.compress(Bitmap.CompressFormat.PNG, 100, bytes))
                            throw new IllegalStateException("PNG encoding failed");
                        nativeResult(request, null, bytes.toByteArray(), null);
                    } finally { bitmap.recycle(); }
                    break;
                default: throw new IllegalArgumentException("unknown preview operation " + operation);
            }
        } catch (RuntimeException error) {
            String message = error.toString();
            if (request != 0) nativeResult(request, null, null, message);
            else nativeEvent(id, 6, "", "", 0, message);
        }
    }

    private void blur(WebView view) {
        if (view.hasFocus()) {
            ((InputMethodManager)activity.getSystemService(Context.INPUT_METHOD_SERVICE))
                    .hideSoftInputFromWindow(view.getWindowToken(), 0);
            view.clearFocus();
        }
    }

    @Override public void onActivityPaused(Activity a) {
        if (a != activity) return;
        resumed = false;
        container.setVisibility(View.GONE);
        for (WebView view : views.values()) view.onPause();
    }
    @Override public void onActivityResumed(Activity a) {
        if (a != activity) return;
        resumed = true;
        container.setVisibility(View.VISIBLE);
        for (WebView view : views.values()) view.onResume();
    }
    @Override public void onActivityDestroyed(Activity a) {
        if (a != activity) return;
        for (Long id : views.keySet().toArray(new Long[0])) command(id, 0, "destroy", "", 0, 0, 0, 0);
        activity.getApplication().unregisterActivityLifecycleCallbacks(this);
    }
    @Override public void onActivityCreated(Activity a, Bundle b) {}
    @Override public void onActivityStarted(Activity a) {}
    @Override public void onActivityStopped(Activity a) {}
    @Override public void onActivitySaveInstanceState(Activity a, Bundle b) {}
}
