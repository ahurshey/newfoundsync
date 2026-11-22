// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

package ca.newfoundsync.tv;

import android.app.Activity;
import android.content.SharedPreferences;
import android.graphics.Color;
import android.graphics.Typeface;
import android.graphics.drawable.GradientDrawable;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.provider.Settings;
import android.text.InputType;
import android.util.TypedValue;
import android.view.Gravity;
import android.view.KeyEvent;
import android.view.View;
import android.view.ViewGroup;
import android.view.WindowManager;
import android.view.inputmethod.EditorInfo;
import android.webkit.WebResourceError;
import android.webkit.WebResourceRequest;
import android.webkit.WebSettings;
import android.webkit.WebView;
import android.webkit.WebViewClient;
import android.widget.Button;
import android.widget.EditText;
import android.widget.FrameLayout;
import android.widget.ImageView;
import android.widget.LinearLayout;
import android.widget.TextView;

import org.json.JSONObject;

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.net.HttpURLConnection;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.net.URL;
import java.net.URLEncoder;
import java.security.SecureRandom;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Enumeration;
import java.util.List;
import java.util.Locale;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;

/**
 * Newfoundsync for Android TV / Google TV.
 *
 * <p>Finds the Newfoundsync server on the network and plays its TV page full screen: the same page a
 * Google Cast device runs ({@code http://<server>:47001/cast}), which starts by itself, takes PCM
 * from the server and joins the sync like every other device. All the sync logic lives in that page,
 * so this app is only the search and a WebView.
 *
 * <p>The search tries the last server first, then every address in this TV's /24 on the TV page's
 * port, asking {@code /hello} whether it is a Newfoundsync server. It does not rely on mDNS: many home
 * networks drop multicast between wired and wireless devices.
 */
public class MainActivity extends Activity {
    /** The server's plain-HTTP port for TVs (its HTTPS port, 47000 by default, plus one). */
    static final int PORT = 47001;
    private static final int LAST_TIMEOUT_MS = 1500;
    private static final int SCAN_TIMEOUT_MS = 900;

    private static final int BG = Color.rgb(0x0b, 0x0f, 0x15);
    private static final int SURFACE = Color.rgb(0x16, 0x1c, 0x26);
    private static final int TEXT = Color.rgb(0xe8, 0xee, 0xf5);
    private static final int DIM = Color.rgb(0x94, 0xa1, 0xb2);
    private static final int ACCENT = Color.rgb(0x3b, 0x8e, 0xff);

    private final Handler ui = new Handler(Looper.getMainLooper());
    private SharedPreferences prefs;
    private FrameLayout root;
    private LinearLayout panel;
    private TextView status;
    private LinearLayout list;
    private EditText manual;
    private WebView web;
    /** Bumped on every new search or connection, so a finished search that is no longer wanted is ignored. */
    private int generation;

    /** A server that answered {@code /hello}. {@code host} is "address" or "address:port". */
    static final class Server {
        final String host;
        final String name;

        Server(String host, String name) {
            this.host = host;
            this.name = name;
        }
    }

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        prefs = getSharedPreferences("newfoundsync", MODE_PRIVATE);
        buildSearchScreen();
        setContentView(root);
        search();
    }

    // ===== the search screen ======================================================================

    private void buildSearchScreen() {
        root = new FrameLayout(this);
        root.setBackgroundColor(BG);

        panel = new LinearLayout(this);
        panel.setOrientation(LinearLayout.VERTICAL);
        panel.setGravity(Gravity.CENTER_HORIZONTAL);
        int pad = dp(48);
        panel.setPadding(pad, pad, pad, pad);

        ImageView logo = new ImageView(this);
        logo.setImageResource(R.mipmap.ic_launcher);
        panel.addView(logo, new LinearLayout.LayoutParams(dp(112), dp(112)));

        TextView title = text("Newfoundsync", 30, TEXT);
        title.setTypeface(Typeface.DEFAULT_BOLD);
        title.setPadding(0, dp(12), 0, dp(6));
        panel.addView(title);

        status = text("", 18, DIM);
        status.setGravity(Gravity.CENTER);
        status.setMaxWidth(dp(640));
        panel.addView(status);

        list = new LinearLayout(this);
        list.setOrientation(LinearLayout.VERTICAL);
        list.setGravity(Gravity.CENTER_HORIZONTAL);
        list.setPadding(0, dp(18), 0, dp(6));
        panel.addView(list);

        LinearLayout row = new LinearLayout(this);
        row.setOrientation(LinearLayout.HORIZONTAL);
        row.setGravity(Gravity.CENTER_VERTICAL);
        row.setPadding(0, dp(18), 0, 0);
        manual = new EditText(this);
        manual.setHint("Server address, like 192.168.1.20");
        manual.setHintTextColor(DIM);
        manual.setTextColor(TEXT);
        manual.setSingleLine(true);
        manual.setInputType(InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_URI);
        manual.setImeOptions(EditorInfo.IME_ACTION_GO);
        manual.setOnEditorActionListener((v, actionId, event) -> {
            connectTyped();
            return true;
        });
        row.addView(manual, new LinearLayout.LayoutParams(dp(340), ViewGroup.LayoutParams.WRAP_CONTENT));
        Button go = button("Connect");
        go.setOnClickListener(v -> connectTyped());
        row.addView(go);
        Button again = button("Search again");
        again.setOnClickListener(v -> search());
        row.addView(again);
        panel.addView(row);

        FrameLayout.LayoutParams lp = new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT, Gravity.CENTER);
        root.addView(panel, lp);
    }

    private void showSearchScreen(String message) {
        if (web != null) {
            root.removeView(web);
            web.destroy();
            web = null;
        }
        panel.setVisibility(View.VISIBLE);
        status.setText(message);
    }

    /** Look for a server: the last one first, then the whole /24. */
    private void search() {
        final int gen = ++generation;
        showSearchScreen("Looking for Newfoundsync on your network…");
        list.removeAllViews();
        final String last = prefs.getString("server", null);
        new Thread(() -> {
            if (last != null) {
                Server s = hello(last, LAST_TIMEOUT_MS);
                if (s != null) {
                    ui.post(() -> { if (gen == generation) play(s); });
                    return;
                }
            }
            List<Server> found = scan();
            ui.post(() -> { if (gen == generation) searchDone(found); });
        }, "search").start();
    }

    private void searchDone(List<Server> found) {
        list.removeAllViews();
        if (found.size() == 1) {
            play(found.get(0));
            return;
        }
        if (found.isEmpty()) {
            status.setText("No Newfoundsync server answered on this network. Open Newfoundsync on your "
                    + "PC, then choose Search again, or type the PC's address.");
            manual.requestFocus();
            return;
        }
        status.setText("Choose a server:");
        for (Server s : found) {
            Button b = button(s.name + "   ·   " + s.host);
            b.setOnClickListener(v -> play(s));
            list.addView(b, new LinearLayout.LayoutParams(dp(520), ViewGroup.LayoutParams.WRAP_CONTENT));
        }
        list.getChildAt(0).requestFocus();
    }

    private void connectTyped() {
        final String typed = manual.getText().toString().trim()
                .replaceFirst("^https?://", "").replaceAll("/.*$", "");
        if (typed.isEmpty()) return;
        final int gen = ++generation;
        status.setText("Trying " + typed + "…");
        new Thread(() -> {
            Server s = hello(typed, 3000);
            ui.post(() -> {
                if (gen != generation) return;
                if (s != null) play(s);
                else status.setText("Nothing at " + typed + " answered as Newfoundsync. Check the address "
                        + "shown in the server window (this app uses port " + PORT + ").");
            });
        }, "typed").start();
    }

    // ===== playing ================================================================================

    private void play(Server s) {
        ++generation;
        prefs.edit().putString("server", s.host).apply();
        panel.setVisibility(View.GONE);

        web = new WebView(this);
        web.setBackgroundColor(BG);
        WebSettings ws = web.getSettings();
        ws.setJavaScriptEnabled(true);
        ws.setDomStorageEnabled(true);
        ws.setMediaPlaybackRequiresUserGesture(false); // nobody is there to tap Start
        final String serverName = s.name;
        web.setWebViewClient(new WebViewClient() {
            @Override
            public void onReceivedError(WebView view, WebResourceRequest req, WebResourceError err) {
                if (!req.isForMainFrame()) return;
                // The page itself rides out a server restart once loaded; this is the page not loading.
                ui.post(() -> {
                    showSearchScreen("Lost " + serverName + ". Looking again…");
                    ui.postDelayed(MainActivity.this::search, 3000);
                });
            }
        });
        root.addView(web, new FrameLayout.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT));
        web.loadUrl("http://" + withPort(s.host) + "/cast?app=1&id=" + enc(deviceId()) + "&name=" + enc(deviceName()));
        web.requestFocus();
    }

    @Override
    public boolean onKeyDown(int keyCode, KeyEvent event) {
        if (keyCode == KeyEvent.KEYCODE_BACK && web != null) {
            // Back from playing goes to the search screen (to pick another server), and from there out.
            ++generation;
            showSearchScreen("Stopped. Choose Search again to play.");
            return true;
        }
        return super.onKeyDown(keyCode, event);
    }

    @Override
    protected void onResume() {
        super.onResume();
        if (web != null) web.onResume();
    }

    @Override
    protected void onPause() {
        if (web != null) web.onPause();
        super.onPause();
    }

    @Override
    protected void onDestroy() {
        ++generation;
        if (web != null) web.destroy();
        super.onDestroy();
    }

    // ===== finding servers ========================================================================

    /** Every Newfoundsync server in this device's /24. */
    private static List<Server> scan() {
        Inet4Address me = lanAddress();
        List<Server> found = new ArrayList<>();
        if (me == null) return found;
        byte[] a = me.getAddress();
        String prefix = (a[0] & 255) + "." + (a[1] & 255) + "." + (a[2] & 255) + ".";
        ExecutorService pool = Executors.newFixedThreadPool(48);
        List<Future<Server>> probes = new ArrayList<>();
        for (int d = 1; d <= 254; d++) {
            if (d == (a[3] & 255)) continue;
            final String ip = prefix + d;
            probes.add(pool.submit(() -> hello(ip, SCAN_TIMEOUT_MS)));
        }
        for (Future<Server> f : probes) {
            try {
                Server s = f.get(10, TimeUnit.SECONDS);
                if (s != null) found.add(s);
            } catch (Exception ignored) {
                // a timeout or a refused connection: nobody there
            }
        }
        pool.shutdownNow();
        Collections.sort(found, (x, y) -> x.name.compareToIgnoreCase(y.name));
        return found;
    }

    /** This device's private IPv4 address (Wi-Fi or Ethernet), or null when it has none. */
    private static Inet4Address lanAddress() {
        try {
            Enumeration<NetworkInterface> ifs = NetworkInterface.getNetworkInterfaces();
            while (ifs != null && ifs.hasMoreElements()) {
                NetworkInterface ni = ifs.nextElement();
                if (!ni.isUp() || ni.isLoopback() || ni.isVirtual()) continue;
                Enumeration<InetAddress> addrs = ni.getInetAddresses();
                while (addrs.hasMoreElements()) {
                    InetAddress ad = addrs.nextElement();
                    if (ad instanceof Inet4Address && ad.isSiteLocalAddress()) return (Inet4Address) ad;
                }
            }
        } catch (Exception ignored) {
            // no interfaces readable: treat as offline
        }
        return null;
    }

    /** The server at {@code host} if it answers {@code /hello} as Newfoundsync, else null. */
    static Server hello(String host, int timeoutMs) {
        HttpURLConnection c = null;
        try {
            c = (HttpURLConnection) new URL("http://" + withPort(host) + "/hello").openConnection();
            c.setConnectTimeout(timeoutMs);
            c.setReadTimeout(timeoutMs);
            c.setUseCaches(false);
            if (c.getResponseCode() != 200) return null;
            ByteArrayOutputStream body = new ByteArrayOutputStream();
            try (InputStream in = c.getInputStream()) {
                byte[] buf = new byte[1024];
                int n;
                while ((n = in.read(buf)) > 0 && body.size() < 8192) body.write(buf, 0, n);
            }
            JSONObject j = new JSONObject(body.toString("UTF-8"));
            if (!"newfoundsync".equals(j.optString("app"))) return null;
            String name = j.optString("name", "").trim();
            return new Server(host, name.isEmpty() ? "Newfoundsync" : name);
        } catch (Exception e) {
            return null;
        } finally {
            if (c != null) c.disconnect();
        }
    }

    private static String withPort(String host) {
        return host.contains(":") ? host : host + ":" + PORT;
    }

    // ===== who this TV is =========================================================================

    /** A stable id for this TV, so the server remembers its name, volume and sync. */
    private String deviceId() {
        String id = prefs.getString("id", null);
        if (id == null) {
            byte[] b = new byte[8];
            new SecureRandom().nextBytes(b);
            StringBuilder sb = new StringBuilder("tv-");
            for (byte x : b) sb.append(String.format(Locale.ROOT, "%02x", x));
            id = sb.toString();
            prefs.edit().putString("id", id).apply();
        }
        return id;
    }

    /** The name the owner gave the TV ("Living Room TV"), else its model. */
    private String deviceName() {
        String name = null;
        try {
            name = Settings.Global.getString(getContentResolver(), Settings.Global.DEVICE_NAME);
        } catch (Exception ignored) {
            // not readable on this build
        }
        if (name == null || name.trim().isEmpty()) name = Build.MODEL;
        return name.trim();
    }

    // ===== small helpers ==========================================================================

    private static String enc(String s) {
        try {
            return URLEncoder.encode(s, "UTF-8");
        } catch (Exception e) {
            return "";
        }
    }

    private int dp(int v) {
        return (int) TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, v, getResources().getDisplayMetrics());
    }

    private TextView text(String s, int sp, int color) {
        TextView t = new TextView(this);
        t.setText(s);
        t.setTextSize(TypedValue.COMPLEX_UNIT_SP, sp);
        t.setTextColor(color);
        return t;
    }

    /** A button that shows clearly where the remote's focus is. */
    private Button button(String label) {
        Button b = new Button(this);
        b.setText(label);
        b.setAllCaps(false);
        b.setTextSize(TypedValue.COMPLEX_UNIT_SP, 17);
        b.setTextColor(TEXT);
        b.setFocusable(true);
        GradientDrawable bg = new GradientDrawable();
        bg.setColor(SURFACE);
        bg.setCornerRadius(dp(10));
        bg.setStroke(dp(2), SURFACE);
        b.setBackground(bg);
        b.setPadding(dp(18), dp(10), dp(18), dp(10));
        b.setOnFocusChangeListener((v, focused) -> bg.setStroke(dp(2), focused ? ACCENT : SURFACE));
        LinearLayout.LayoutParams lp = new LinearLayout.LayoutParams(
                ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.WRAP_CONTENT);
        lp.setMargins(dp(6), dp(6), dp(6), dp(6));
        b.setLayoutParams(lp);
        return b;
    }
}
