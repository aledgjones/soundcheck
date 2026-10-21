package dev.soundcheck.reader;

import android.accessibilityservice.AccessibilityServiceInfo;
import android.app.Instrumentation;
import android.app.UiAutomation;
import android.graphics.Rect;
import android.os.Bundle;
import android.view.WindowManager;
import android.view.accessibility.AccessibilityNodeInfo;
import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.TimeoutException;

// Keeps one UiAutomation connection open and serves the active window's tree as `uiautomator dump` XML on
// 127.0.0.1:9018 (GET /dump, GET /dump?all, GET /ping answering "ok soundcheck"), so each read skips uiautomator's ~3.5 s start-up. Emulator-only test tool.
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
    // The same flags uiautomator sets for an uncompressed dump.
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
      node(xml, root, 0, display, all);
    }
    xml.append("</hierarchy>");
    return xml.toString();
  }

  private Rect display = new Rect();

  private void node(StringBuilder xml, AccessibilityNodeInfo n, int index, Rect parentVisible, boolean all) {
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
      // uiautomator skips children that aren't visible to the user. Known gap: the annotation text box's Done, move
      // and resize controls report invisible to this connection though uiautomator lists them (2026-10-20).
      if (child != null && (all || child.isVisibleToUser())) {
        node(xml, child, childIndex++, bounds, all);
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
