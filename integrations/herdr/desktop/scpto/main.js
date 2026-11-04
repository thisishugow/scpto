// A desktop entry point for the unchanged scpto terminal interface.

import { View, div } from "gpui-kit";
import { v_flex } from "gpui-base";
import { hc, run, spaces, focused, home, panes, terminals, theme, props, closeOverlay } from "herdr";
import { createLauncher } from "./session.js";

export default class Scpto extends View {
  init(_props, cx) {
    this.launch = createLauncher({ hc, run, spaces, focused, home, panes, terminals });
    this.busy = false;
    this.error = null;
    // Checks and old layout blocks must not start a terminal on load.
    if (props()?.overlay) this.open(cx);
  }

  open(cx) {
    if (this.busy) return;
    const overlay = props()?.overlay;
    this.busy = true;
    this.error = null;
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
        this.error = error.message;
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
    return v_flex().size_full().p(16).gap(12)
      .bg(t.popover).text_color(t.foreground).text_size(13)
      .child(div().font_semibold()
        .child(this.error ? "Cannot open scpto" : this.busy ? "Opening scpto..." : "scpto"))
      .when(Boolean(this.error), (el) => el
        .child(div().id("launch-error").flex_1().min_h(0).overflow_y_scrollbar()
          .whitespace_normal().text_color(t.muted).child(this.error))
        .child(div().id("retry-scpto").px(12).py(7).rounded(6).self_start()
          .bg(t.solid).text_color(t.solid_foreground).font_semibold()
          .cursor_pointer().hover((style) => style.opacity(0.85))
          .on_click((_event, cx) => this.open(cx)).child("Retry")));
  }
}
