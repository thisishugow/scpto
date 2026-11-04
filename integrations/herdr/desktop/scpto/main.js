// A desktop entry point for the unchanged scpto terminal interface.

import { View, div } from "gpui-kit";
import { h_flex } from "gpui-base";
import { hc, run, spaces, focused, home, panes, terminals, theme, notify, nextCommand, props, closeOverlay } from "herdr";
import { createLauncher } from "./session.js";

export default class Scpto extends View {
  init(_props, cx) {
    this.launch = createLauncher({ hc, run, spaces, focused, home, panes, terminals });
    this.busy = false;
    cx.spawn(async (cx) => {
      for (;;) {
        const command = await nextCommand();
        if (command === "open") this.open(cx);
      }
    });
  }

  open(cx) {
    if (this.busy) return;
    const overlay = props()?.overlay;
    this.busy = true;
    cx.notify();
    cx.spawn(async (cx) => {
      try {
        await this.launch(async (ready, message) => {
          for (let attempt = 0; attempt < 100; attempt += 1) {
            if (ready()) return;
            await cx.sleep(50);
          }
          throw new Error(message);
        });
      } catch (error) {
        notify({ title: "Cannot open scpto", body: error.message, level: "error" });
        return;
      } finally {
        this.busy = false;
        cx.notify();
      }
      if (overlay) closeOverlay(overlay);
    });
  }

  render() {
    const t = theme();
    return h_flex().size_full().px(12).gap(12).items_center()
      .bg(t.surface).text_color(t.foreground)
      .child(div().id("open-scpto").px(12).py(7).rounded(6)
        .bg(t.solid).text_color(t.solid_foreground).text_size(12).font_semibold()
        .cursor_pointer().hover((style) => style.opacity(0.85))
        .on_click((_event, cx) => this.open(cx))
        .child(this.busy ? "Opening..." : "Open scpto"))
      .child(div().flex_1().min_w(0).truncate().text_size(12).text_color(t.muted)
        .child("Local and SFTP files. Opens or focuses your scpto session."));
  }
}
