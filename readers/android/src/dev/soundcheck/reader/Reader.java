package dev.soundcheck.reader;

import android.accessibilityservice.AccessibilityServiceInfo;
import android.app.Instrumentation;
import android.app.UiAutomation;
import android.graphics.Rect;
import android.os.Bundle;
import android.os.SystemClock;
import android.view.InputDevice;
import android.view.KeyCharacterMap;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.WindowManager;
import android.view.accessibility.AccessibilityNodeInfo;
import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.URLDecoder;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.TimeoutException;

// Keeps one UiAutomation connection open and serves the active window's tree as `uiautomator dump` XML on
// 127.0.0.1:9018 (GET /dump, GET /dump?all, GET /ping answering "ok soundcheck"), so each read skips uiautomator's ~3.5 s start-up.
// Input goes through the same connection (GET /input?<script>, GET /type?<text>, GET /release[?back]) rather than an `adb shell input` per
// action. Emulator-only test tool.
public class Reader extends Instrumentation {
  private static final int PORT = 9018;
  private UiAutomation automation;

  @Override
  public void onCreate(Bundle arguments) {
    super.onCreate(arguments);
    start();
  }

  @Override
  public void onStart() {
    automation = getUiAutomation(UiAutomation.FLAG_DONT_SUPPRESS_ACCESSIBILITY_SERVICES);
    AccessibilityServiceInfo info = automation.getServiceInfo();
    // uiautomator's flags for an uncompressed dump, plus interactive windows, as TalkBack asks: with it, Android counts
    // a node the keyboard covers as not visible to the user, so a tap can't land on the keyboard instead.
    info.flags |=
        AccessibilityServiceInfo.FLAG_INCLUDE_NOT_IMPORTANT_VIEWS
            | AccessibilityServiceInfo.FLAG_REPORT_VIEW_IDS
            | AccessibilityServiceInfo.FLAG_RETRIEVE_INTERACTIVE_WINDOWS;
    automation.setServiceInfo(info);
    try (ServerSocket server = new ServerSocket(PORT, 8, InetAddress.getByName("127.0.0.1"))) {
      while (true) {
        try (Socket socket = server.accept()) {
          serve(socket);
        } catch (Exception e) {
          // One bad request must not stop the reader.
        }
      }
    } catch (Exception e) {
      Bundle result = new Bundle();
      result.putString("error", String.valueOf(e));
      finish(1, result);
    }
  }

  private void serve(Socket socket) throws Exception {
    BufferedReader in =
        new BufferedReader(new InputStreamReader(socket.getInputStream(), StandardCharsets.UTF_8));
    String request = in.readLine();
    String body;
    int status = 200;
    if (request != null && request.startsWith("GET /dump")) {
      try {
        body = dump(request.startsWith("GET /dump?all"));
      } catch (Exception e) {
        status = 500;
        body = String.valueOf(e);
      }
    } else if (request != null
        && (request.startsWith("GET /input?") || request.startsWith("GET /type?"))) {
      String query = request.substring(request.indexOf('?') + 1);
      int end = query.indexOf(' ');
      query = URLDecoder.decode(end < 0 ? query : query.substring(0, end), StandardCharsets.UTF_8);
      try {
        if (request.startsWith("GET /input?")) {
          input(query);
        } else {
          type(query);
        }
        body = "ok";
      } catch (Exception e) {
        status = 500;
        body = String.valueOf(e);
      }
    } else if (request != null && request.startsWith("GET /release")) {
      try {
        release(request.startsWith("GET /release?back"));
        body = "ok";
      } catch (Exception e) {
        status = 500;
        body = String.valueOf(e);
      }
    } else if (request != null && request.startsWith("GET /ping")) {
      body = "ok soundcheck";
    } else {
      status = 404;
      body = "not found";
    }
    byte[] bytes = body.getBytes(StandardCharsets.UTF_8);
    OutputStream out = socket.getOutputStream();
    out.write(
        ("HTTP/1.0 " + status + " OK\r\nContent-Type: text/xml; charset=utf-8\r\nContent-Length: "
                + bytes.length + "\r\nConnection: close\r\n\r\n")
            .getBytes(StandardCharsets.UTF_8));
    out.write(bytes);
    out.flush();
  }

  // When the touch went down, or 0 when none is down; a second `d` moves it.
  private long downTime;
  private float originX, originY, lastX, lastY;
  // Whether a script's closing `h` left the touch down for `release`; holdId ends the old hold's timeout.
  private boolean held;
  private int holdId;

  // A script of steps separated by `;` (the same as the iOS reader's): `d x y` touch down, or move while down; `u x y`
  // touch up; `w ms` wait; `k code` key press; `kd code` / `ku code` key down and up; and a last `h` that leaves the
  // touch down until `release`. Codes are Android key codes. A touch still held goes back to its start and lifts first.
  private synchronized void input(String script) throws Exception {
    if (held) {
      lift(true);
    }
    String[] steps = script.split(";");
    for (int i = 0; i < steps.length; i++) {
      String step = steps[i];
      String[] p = step.trim().split(" ");
      switch (p[0]) {
        case "d" -> touch(downTime == 0 ? MotionEvent.ACTION_DOWN : MotionEvent.ACTION_MOVE, p);
        case "u" -> touch(MotionEvent.ACTION_UP, p);
        case "h" -> {
          if (i != steps.length - 1 || downTime == 0) {
            throw new IllegalArgumentException("`h` ends a script whose touch is down");
          }
          hold();
        }
        case "w" -> Thread.sleep(Long.parseLong(p[1]));
        case "k" -> {
          key(KeyEvent.ACTION_DOWN, Integer.parseInt(p[1]));
          key(KeyEvent.ACTION_UP, Integer.parseInt(p[1]));
        }
        case "kd" -> key(KeyEvent.ACTION_DOWN, Integer.parseInt(p[1]));
        case "ku" -> key(KeyEvent.ACTION_UP, Integer.parseInt(p[1]));
        default -> throw new IllegalArgumentException("bad step: " + step);
      }
    }
  }

  // Unlike iOS, Android never ends a touch held still, so a hold needs no keep-alive, only the 30 s timeout.
  private void hold() {
    held = true;
    int id = ++holdId;
    Thread timeout =
        new Thread(
            () -> {
              try {
                Thread.sleep(30_000);
                synchronized (this) {
                  if (held && holdId == id) {
                    lift(true);
                  }
                }
              } catch (Exception e) {
                // The hold was released or the reader is stopping.
              }
            });
    timeout.setDaemon(true);
    timeout.start();
  }

  private synchronized void release(boolean back) throws Exception {
    if (!held) {
      throw new IllegalStateException("No touch held");
    }
    lift(back);
  }

  // Lifts the held touch where it is, or `back` along a straight line to where it began, so nothing is dropped.
  private void lift(boolean back) throws Exception {
    held = false;
    holdId++;
    if (back) {
      int steps =
          Math.max(1, (int) Math.ceil(Math.max(Math.abs(originX - lastX), Math.abs(originY - lastY)) / 10));
      float fromX = lastX, fromY = lastY;
      for (int i = 1; i <= steps; i++) {
        float t = (float) i / steps;
        touch(MotionEvent.ACTION_MOVE, fromX + (originX - fromX) * t, fromY + (originY - fromY) * t);
        Thread.sleep(16);
      }
    }
    touch(MotionEvent.ACTION_UP, lastX, lastY);
  }

  private void touch(int action, String[] p) {
    touch(action, Float.parseFloat(p[1]), Float.parseFloat(p[2]));
  }

  private void touch(int action, float x, float y) {
    long now = SystemClock.uptimeMillis();
    if (action == MotionEvent.ACTION_DOWN) {
      downTime = now;
      originX = x;
      originY = y;
    }
    lastX = x;
    lastY = y;
    // A finger, as `adb shell input` sends: the short obtain() leaves the tool type unknown, and the system file
    // picker routes gestures by tool type, so it showed a ripple but never selected.
    MotionEvent.PointerProperties[] props = {new MotionEvent.PointerProperties()};
    props[0].id = 0;
    props[0].toolType = MotionEvent.TOOL_TYPE_FINGER;
    MotionEvent.PointerCoords[] coords = {new MotionEvent.PointerCoords()};
    coords[0].x = x;
    coords[0].y = y;
    coords[0].pressure = 1;
    coords[0].size = 1;
    MotionEvent e =
        MotionEvent.obtain(downTime, now, action, 1, props, coords, 0, 0, 1, 1, touchscreen(), 0,
            InputDevice.SOURCE_TOUCHSCREEN, 0);
    automation.injectInputEvent(e, true);
    e.recycle();
    if (action == MotionEvent.ACTION_UP) {
      downTime = 0;
    }
  }

  private int touchscreenId = -1;

  // The device id of a real touchscreen, so views that ask which device a touch came from get an answer.
  private int touchscreen() {
    if (touchscreenId < 0) {
      touchscreenId = 0;
      for (int id : InputDevice.getDeviceIds()) {
        InputDevice d = InputDevice.getDevice(id);
        if (d != null && d.supportsSource(InputDevice.SOURCE_TOUCHSCREEN)) {
          touchscreenId = id;
          break;
        }
      }
    }
    return touchscreenId;
  }

  private void key(int action, int code) {
    long now = SystemClock.uptimeMillis();
    automation.injectInputEvent(new KeyEvent(now, now, action, code, 0), true);
  }

  // Each character as the keys that type it (shift included), from the device's own key map. Keys sent back to back
  // were dropped by a field that re-renders as it changes, so each key down waits a moment.
  private void type(String text) throws Exception {
    KeyEvent[] events =
        KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD).getEvents(text.toCharArray());
    if (events == null) {
      throw new IllegalArgumentException("no keys type " + text);
    }
    for (KeyEvent e : events) {
      if (e.getAction() == KeyEvent.ACTION_DOWN) {
        Thread.sleep(30);
      }
      automation.injectInputEvent(KeyEvent.changeTimeRepeat(e, SystemClock.uptimeMillis(), 0), true);
    }
  }

  private String dump(boolean all) {
    // The default view is the tree a screen reader is given: without views marked not important, as Android itself
    // decides importance (`uiautomator dump --compressed`). `all` asks for everything, as `uiautomator dump` does.
    AccessibilityServiceInfo info = automation.getServiceInfo();
    if (all) {
      info.flags |= AccessibilityServiceInfo.FLAG_INCLUDE_NOT_IMPORTANT_VIEWS;
    } else {
      info.flags &= ~AccessibilityServiceInfo.FLAG_INCLUDE_NOT_IMPORTANT_VIEWS;
    }
    automation.setServiceInfo(info);
    try {
      // Unlike uiautomator, never fail on a screen that never settles (a pulsing coach mark): read it anyway.
      automation.waitForIdle(100, 1500);
    } catch (TimeoutException e) {
      // Fall through.
    }
    // A long-lived connection keeps an accessibility cache that can hold stale nodes (an overlay the app didn't
    // announce read as invisible); uiautomator never sees that because it connects afresh, so start each read clean.
    automation.clearCache();
    // Straight after a launch or a settings change there can briefly be no active window: wait for one.
    AccessibilityNodeInfo root = automation.getRootInActiveWindow();
    for (int i = 0; root == null && i < 20; i++) {
      try {
        Thread.sleep(100);
      } catch (InterruptedException e) {
        break;
      }
      root = automation.getRootInActiveWindow();
    }
    WindowManager wm = (WindowManager) getContext().getSystemService(WindowManager.class);
    Rect metrics = wm.getCurrentWindowMetrics().getBounds();
    @SuppressWarnings("deprecation")
    int rotation = wm.getDefaultDisplay().getRotation();
    StringBuilder xml = new StringBuilder("<?xml version='1.0' encoding='UTF-8' standalone='yes' ?>");
    xml.append("<hierarchy rotation=\"").append(rotation).append("\">");
    if (root != null) {
      display = new Rect(0, 0, metrics.width(), metrics.height());
      node(xml, root, 0, display);
    }
    xml.append("</hierarchy>");
    return xml.toString();
  }

  private Rect display = new Rect();

  private void node(StringBuilder xml, AccessibilityNodeInfo n, int index, Rect parentVisible) {
    Rect bounds = new Rect();
    n.getBoundsInScreen(bounds);
    // uiautomator reports bounds clipped to what is on screen; a tap at the centre must land on the node.
    if (!bounds.intersect(parentVisible)) {
      bounds.setEmpty();
    }
    xml.append("<node index=\"").append(index).append('"');
    attr(xml, "text", n.getText());
    attr(xml, "resource-id", n.getViewIdResourceName());
    attr(xml, "class", n.getClassName());
    attr(xml, "package", n.getPackageName());
    attr(xml, "content-desc", n.getContentDescription());
    flag(xml, "checkable", n.isCheckable());
    flag(xml, "checked", n.isChecked());
    flag(xml, "clickable", n.isClickable());
    flag(xml, "enabled", n.isEnabled());
    flag(xml, "focusable", n.isFocusable());
    flag(xml, "focused", n.isFocused());
    flag(xml, "scrollable", n.isScrollable());
    flag(xml, "long-clickable", n.isLongClickable());
    flag(xml, "password", n.isPassword());
    flag(xml, "selected", n.isSelected());
    xml.append(" bounds=\"[").append(bounds.left).append(',').append(bounds.top).append("][")
        .append(bounds.right).append(',').append(bounds.bottom).append("]\">");
    int childIndex = 0;
    for (int i = 0; i < n.getChildCount(); i++) {
      AccessibilityNodeInfo child = n.getChild(i);
      // uiautomator skips children that aren't visible to the user, even in a full dump. A control drawn outside its
      // parent's bounds counts as invisible, as it does for TalkBack (the annotation text box's Done, 2026-10-21).
      if (child != null && child.isVisibleToUser()) {
        node(xml, child, childIndex++, bounds);
      }
    }
    xml.append("</node>");
  }

  private static void attr(StringBuilder xml, String name, CharSequence value) {
    xml.append(' ').append(name).append("=\"").append(escape(value == null ? "" : value.toString()))
        .append('"');
  }

  private static void flag(StringBuilder xml, String name, boolean value) {
    xml.append(' ').append(name).append("=\"").append(value).append('"');
  }

  private static String escape(String s) {
    StringBuilder out = new StringBuilder(s.length());
    for (int i = 0; i < s.length(); i++) {
      char c = s.charAt(i);
      switch (c) {
        case '&': out.append("&amp;"); break;
        case '<': out.append("&lt;"); break;
        case '>': out.append("&gt;"); break;
        case '"': out.append("&quot;"); break;
        case '\'': out.append("&apos;"); break;
        case '\n': out.append("&#10;"); break;
        case '\r': out.append("&#13;"); break;
        default:
          // XML 1.0 can't carry most control characters.
          if (c >= 0x20 || c == '\t') {
            out.append(c);
          }
      }
    }
    return out.toString();
  }
}
