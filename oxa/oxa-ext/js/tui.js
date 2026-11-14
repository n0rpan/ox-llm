// oxa:tui, the pi-tui class shim for extensions.
// ported from pi packages/tui/src/components/{text,box,spacer,select-list,input,container}.ts,
// packages/tui/src/keys.ts and utils.ts (visibleWidth, truncateToWidth) (MIT)
//
// Shape: every component has render(width) -> string[] and handleInput(key, text). `key` is an
// oxa key id ("up", "enter", "ctrl+c", "a"), `text` the typed text or null. Lines are plain
// strings; SGR escapes are allowed and cost no columns. The host cuts any line that is still
// too wide, so these classes never need to be exact about the last column.

const ESC_RE = /\x1b\[[0-?]*[ -\/]*[@-~]/g;

// Columns of one code point (same table as oxa-ext's Rust side).
function cpWidth(cp) {
  if (cp < 0x20 || (cp >= 0x7f && cp < 0xa0)) return 0;
  if ((cp >= 0x300 && cp <= 0x36f) || (cp >= 0x200b && cp <= 0x200f) ||
      (cp >= 0xfe00 && cp <= 0xfe0f) || (cp >= 0x20d0 && cp <= 0x20ff)) return 0;
  if ((cp >= 0x1100 && cp <= 0x115f) || (cp >= 0x2e80 && cp <= 0xa4cf) ||
      (cp >= 0xac00 && cp <= 0xd7a3) || (cp >= 0xf900 && cp <= 0xfaff) ||
      (cp >= 0xfe30 && cp <= 0xfe4f) || (cp >= 0xff00 && cp <= 0xff60) ||
      (cp >= 0xffe0 && cp <= 0xffe6) || (cp >= 0x1f300 && cp <= 0x1f64f) ||
      (cp >= 0x1f900 && cp <= 0x1f9ff) || (cp >= 0x20000 && cp <= 0x3fffd)) return 2;
  return 1;
}

// Split a string into escape sequences and plain text.
function splitAnsi(s) {
  const out = [];
  let last = 0;
  const re = new RegExp(ESC_RE.source, "g");
  let m;
  while ((m = re.exec(s)) !== null) {
    if (m.index > last) out.push({ esc: false, text: s.slice(last, m.index) });
    out.push({ esc: true, text: m[0] });
    last = m.index + m[0].length;
  }
  if (last < s.length) out.push({ esc: false, text: s.slice(last) });
  return out;
}

export function visibleWidth(s) {
  let w = 0;
  for (const seg of splitAnsi(String(s))) {
    if (seg.esc) continue;
    for (const ch of seg.text) w += cpWidth(ch.codePointAt(0));
  }
  return w;
}

export function truncateToWidth(s, width, ellipsis) {
  s = String(s);
  if (ellipsis === undefined) ellipsis = "...";
  if (visibleWidth(s) <= width) return s;
  let ell = ellipsis;
  if (visibleWidth(ell) > width) ell = "";
  const budget = Math.max(0, width - visibleWidth(ell));
  let used = 0;
  let out = "";
  let sawEsc = false;
  let cut = false;
  for (const seg of splitAnsi(s)) {
    if (seg.esc) {
      sawEsc = true;
      out += seg.text;
      continue;
    }
    for (const ch of seg.text) {
      const w = cpWidth(ch.codePointAt(0));
      if (used + w > budget) {
        cut = true;
        break;
      }
      out += ch;
      used += w;
    }
    if (cut) break;
  }
  return out + ell + (sawEsc ? "\x1b[0m" : "");
}

// Word-wrap one line to `width` columns. Words longer than the width are split by code point.
function wrapLine(line, width) {
  if (visibleWidth(line) <= width) return [line];
  const out = [];
  let cur = "";
  for (const word of line.split(" ")) {
    const candidate = cur === "" ? word : cur + " " + word;
    if (visibleWidth(candidate) <= width) {
      cur = candidate;
      continue;
    }
    if (cur !== "") out.push(cur);
    let rest = word;
    while (visibleWidth(rest) > width) {
      const cps = Array.from(rest);
      let acc = "";
      let w = 0;
      let i = 0;
      for (; i < cps.length; i++) {
        const cw = cpWidth(cps[i].codePointAt(0));
        if (w + cw > width) break;
        acc += cps[i];
        w += cw;
      }
      if (i === 0) i = 1;
      out.push(cps.slice(0, i).join(""));
      rest = cps.slice(i).join("");
    }
    cur = rest;
  }
  out.push(cur);
  return out;
}

// Normalised key id: lower case, trimmed, pi's aliases mapped.
function normKey(k) {
  if (k === null || k === undefined) return "";
  let s = String(k).trim().toLowerCase();
  const i = s.lastIndexOf("+");
  let mods = "";
  let key = s;
  if (s === "+") {
    key = "+";
  } else if (s.endsWith("++")) {
    mods = s.slice(0, -2) + "+";
    key = "+";
  } else if (i > 0 && i < s.length - 1) {
    mods = s.slice(0, i + 1);
    key = s.slice(i + 1);
  }
  if (key === "return") key = "enter";
  if (key === "esc") key = "escape";
  if (key === "plus") key = "+";
  return mods + key;
}

export const Key = {
  up: "up",
  down: "down",
  left: "left",
  right: "right",
  enter: "enter",
  return: "enter",
  escape: "escape",
  tab: "tab",
  space: "space",
  backspace: "backspace",
  delete: "delete",
  home: "home",
  end: "end",
  pageUp: "pageup",
  pageDown: "pagedown",
  ctrl: (k) => "ctrl+" + k,
  shift: (k) => "shift+" + k,
  alt: (k) => "alt+" + k,
};

// True when the key id `data` (from handleInput) is the key `key`.
export function matchesKey(data, key) {
  return normKey(data) === normKey(key);
}

export class Text {
  constructor(text = "", paddingX = 1, paddingY = 1) {
    this.text = String(text);
    this.paddingX = paddingX;
    this.paddingY = paddingY;
  }
  setText(text) {
    this.text = String(text);
  }
  invalidate() {}
  render(width) {
    if (this.text.trim() === "") return [];
    const px = Math.min(this.paddingX, Math.max(0, Math.floor((width - 1) / 2)));
    const contentWidth = Math.max(1, width - px * 2);
    const pad = " ".repeat(px);
    const body = [];
    for (const para of this.text.replace(/\t/g, "   ").split("\n")) {
      for (const line of wrapLine(para, contentWidth)) body.push(pad + line);
    }
    const blank = Array(Math.max(0, this.paddingY)).fill("");
    return blank.concat(body, blank);
  }
}

export class Spacer {
  constructor(lines = 1) {
    this.lines = lines;
  }
  invalidate() {}
  render() {
    return Array(Math.max(0, this.lines)).fill("");
  }
}

export class Container {
  constructor() {
    this.children = [];
  }
  addChild(child) {
    this.children.push(child);
  }
  removeChild(child) {
    const i = this.children.indexOf(child);
    if (i !== -1) this.children.splice(i, 1);
  }
  clear() {
    this.children = [];
  }
  invalidate() {
    for (const c of this.children) if (c.invalidate) c.invalidate();
  }
  render(width) {
    const out = [];
    for (const c of this.children) for (const line of c.render(width)) out.push(line);
    return out;
  }
  handleInput(key, text) {
    // Input goes to the last child that takes it (pi's focus model, simplified).
    for (let i = this.children.length - 1; i >= 0; i--) {
      const c = this.children[i];
      if (c.handleInput && c.handleInput(key, text) !== false) return true;
    }
    return false;
  }
}

export class Box {
  constructor(paddingX = 1, paddingY = 1) {
    this.children = [];
    this.paddingX = paddingX;
    this.paddingY = paddingY;
  }
  addChild(child) {
    this.children.push(child);
  }
  removeChild(child) {
    const i = this.children.indexOf(child);
    if (i !== -1) this.children.splice(i, 1);
  }
  clear() {
    this.children = [];
  }
  setPaddingX(px) {
    this.paddingX = px;
  }
  invalidate() {
    for (const c of this.children) if (c.invalidate) c.invalidate();
  }
  render(width) {
    if (this.children.length === 0) return [];
    const contentWidth = Math.max(1, width - this.paddingX * 2);
    const pad = " ".repeat(Math.max(0, this.paddingX));
    const body = [];
    for (const c of this.children) {
      for (const line of c.render(contentWidth)) body.push(pad + line);
    }
    const blank = Array(Math.max(0, this.paddingY)).fill("");
    return blank.concat(body, blank);
  }
}

export class SelectList {
  constructor(items = [], maxVisible = 5) {
    this.items = items.slice();
    this.filtered = this.items;
    this.maxVisible = Math.max(1, maxVisible);
    this.selectedIndex = 0;
    this.onSelect = undefined;
    this.onCancel = undefined;
    this.onSelectionChange = undefined;
  }
  setFilter(filter) {
    const f = String(filter).toLowerCase();
    this.filtered = this.items.filter((it) => it.value.toLowerCase().startsWith(f));
    this.selectedIndex = 0;
  }
  setSelectedIndex(index) {
    this.selectedIndex = Math.max(0, Math.min(index, this.filtered.length - 1));
  }
  getSelectedItem() {
    return this.filtered[this.selectedIndex];
  }
  invalidate() {}
  visibleRange() {
    const start = Math.max(
      0,
      Math.min(this.selectedIndex - Math.floor(this.maxVisible / 2), this.filtered.length - this.maxVisible),
    );
    return { start, end: Math.min(start + this.maxVisible, this.filtered.length) };
  }
  render(width) {
    if (this.filtered.length === 0) return ["  No matching items"];
    const labelWidth = Math.min(
      32,
      Math.max(...this.filtered.map((it) => visibleWidth(it.label))),
    );
    const { start, end } = this.visibleRange();
    const lines = [];
    for (let i = start; i < end; i++) {
      const item = this.filtered[i];
      const prefix = i === this.selectedIndex ? "→ " : "  ";
      let line = prefix + item.label;
      if (item.description) {
        const gap = Math.max(0, labelWidth - visibleWidth(item.label)) + 2;
        line += " ".repeat(gap) + String(item.description).replace(/[\r\n]+/g, " ").trim();
      }
      lines.push(truncateToWidth(line, width, "..."));
    }
    if (start > 0 || end < this.filtered.length) {
      const info = `  (${this.selectedIndex + 1}/${this.filtered.length})`;
      lines.push(truncateToWidth(info, width, ""));
    }
    return lines;
  }
  handleInput(key) {
    const k = normKey(key);
    const n = this.filtered.length;
    if (k === "up") {
      if (n === 0) return;
      this.selectedIndex = this.selectedIndex === 0 ? n - 1 : this.selectedIndex - 1;
      if (this.onSelectionChange) this.onSelectionChange(this.getSelectedItem());
    } else if (k === "down") {
      if (n === 0) return;
      this.selectedIndex = this.selectedIndex === n - 1 ? 0 : this.selectedIndex + 1;
      if (this.onSelectionChange) this.onSelectionChange(this.getSelectedItem());
    } else if (k === "pageup") {
      this.setSelectedIndex(Math.max(0, this.selectedIndex - this.maxVisible));
    } else if (k === "pagedown") {
      this.setSelectedIndex(this.selectedIndex + this.maxVisible);
    } else if (k === "enter") {
      const item = this.getSelectedItem();
      if (item && this.onSelect) this.onSelect(item);
    } else if (k === "escape" || k === "ctrl+c") {
      if (this.onCancel) this.onCancel();
    }
  }
}

export class Input {
  constructor() {
    this.chars = [];
    this.cursor = 0;
    this.onSubmit = undefined;
    this.onEscape = undefined;
  }
  getValue() {
    return this.chars.join("");
  }
  setValue(value) {
    this.chars = Array.from(String(value));
    this.cursor = this.chars.length;
  }
  invalidate() {}
  render(width) {
    const line = "> " + this.chars.slice(0, this.cursor).join("") + "|" + this.chars.slice(this.cursor).join("");
    return [truncateToWidth(line, width, "")];
  }
  handleInput(key, text) {
    const k = normKey(key);
    if (k === "enter") {
      if (this.onSubmit) this.onSubmit(this.getValue());
    } else if (k === "escape") {
      if (this.onEscape) this.onEscape();
    } else if (k === "backspace") {
      if (this.cursor > 0) {
        this.chars.splice(this.cursor - 1, 1);
        this.cursor -= 1;
      }
    } else if (k === "delete") {
      this.chars.splice(this.cursor, 1);
    } else if (k === "left") {
      this.cursor = Math.max(0, this.cursor - 1);
    } else if (k === "right") {
      this.cursor = Math.min(this.chars.length, this.cursor + 1);
    } else if (k === "home") {
      this.cursor = 0;
    } else if (k === "end") {
      this.cursor = this.chars.length;
    } else if (typeof text === "string" && text.length > 0 && !/(^|\+)(ctrl|alt|super)\+/.test(k)) {
      const add = Array.from(text);
      this.chars.splice(this.cursor, 0, ...add);
      this.cursor += add.length;
    }
  }
}

// Word-wrap text to `width` columns, keeping SGR escapes (pi's wrapTextWithAnsi). Newlines start
// a new line; the escapes cost no columns.
export function wrapTextWithAnsi(text, width) {
  if (!text) return [""];
  const out = [];
  for (const line of String(text).split("\n")) {
    for (const part of wrapLine(line, Math.max(1, width))) out.push(part);
  }
  return out;
}

// pi's SettingsList: label / value rows; Enter or Space cycles `values` or opens `submenu`;
// Escape cancels. `theme` is pi's SettingsListTheme (label, value, description, cursor, hint);
// any part may be omitted.
export class SettingsList {
  constructor(items, maxVisible, theme, onChange, onCancel, options) {
    this.capturesViewportInput = true;
    this.items = items || [];
    this.filteredItems = this.items;
    this.maxVisible = Math.max(1, maxVisible || 10);
    const t = theme || {};
    const id = (s) => s;
    this.theme = {
      label: t.label || id,
      value: t.value || id,
      description: t.description || id,
      cursor: t.cursor === undefined ? "→ " : t.cursor,
      hint: t.hint || id,
    };
    this.onChange = onChange || (() => {});
    this.onCancel = onCancel || (() => {});
    this.searchEnabled = !!(options && options.enableSearch);
    this.searchInput = this.searchEnabled ? new Input() : null;
    this.selectedIndex = 0;
    this.submenuComponent = null;
    this.submenuItemIndex = null;
  }
  updateValue(id, newValue) {
    const item = this.items.find((i) => i.id === id);
    if (item) item.currentValue = newValue;
  }
  selectItem(id) {
    const i = this.displayItems().findIndex((it) => it.id === id);
    if (i !== -1) this.selectedIndex = i;
  }
  displayItems() {
    return this.searchEnabled ? this.filteredItems : this.items;
  }
  invalidate() {
    if (this.submenuComponent && this.submenuComponent.invalidate) this.submenuComponent.invalidate();
  }
  render(width) {
    if (this.submenuComponent) return this.submenuComponent.render(width);
    const lines = [];
    if (this.searchInput) {
      lines.push(...this.searchInput.render(width));
      lines.push("");
    }
    const hint = this.searchEnabled
      ? "  Type to search · Enter/Space to change · Esc to cancel"
      : "  Enter/Space to change · Esc to cancel";
    if (this.items.length === 0) {
      lines.push(this.theme.hint("  No settings available"));
      return lines;
    }
    const items = this.displayItems();
    if (items.length === 0) {
      lines.push(truncateToWidth(this.theme.hint("  No matching settings"), width));
      lines.push("", truncateToWidth(this.theme.hint(hint), width));
      return lines;
    }
    const start = Math.max(
      0,
      Math.min(this.selectedIndex - Math.floor(this.maxVisible / 2), items.length - this.maxVisible),
    );
    const end = Math.min(start + this.maxVisible, items.length);
    const labelWidth = Math.min(36, Math.max(...this.items.map((it) => visibleWidth(it.label))));
    for (let i = start; i < end; i++) {
      const item = items[i];
      const selected = i === this.selectedIndex;
      const prefix = selected ? this.theme.cursor : "  ";
      const label = item.label + " ".repeat(Math.max(0, labelWidth - visibleWidth(item.label)));
      const valueWidth = width - visibleWidth(prefix) - labelWidth - 4;
      const value = this.theme.value(truncateToWidth(String(item.currentValue), Math.max(0, valueWidth), ""), selected);
      lines.push(truncateToWidth(prefix + this.theme.label(label, selected) + "  " + value, width));
    }
    if (start > 0 || end < items.length) {
      lines.push(this.theme.hint(truncateToWidth(`  (${this.selectedIndex + 1}/${items.length})`, width - 2, "")));
    }
    const sel = items[this.selectedIndex];
    if (sel && sel.description) {
      lines.push("");
      for (const l of wrapTextWithAnsi(sel.description, width - 4)) lines.push(this.theme.description("  " + l));
    }
    lines.push("", truncateToWidth(this.theme.hint(hint), width));
    return lines;
  }
  handleInput(key, text) {
    if (this.submenuComponent) {
      if (this.submenuComponent.handleInput) this.submenuComponent.handleInput(key, text);
      return;
    }
    const k = normKey(key);
    const items = this.displayItems();
    if (k === "up") {
      if (items.length) this.selectedIndex = this.selectedIndex === 0 ? items.length - 1 : this.selectedIndex - 1;
    } else if (k === "down") {
      if (items.length) this.selectedIndex = this.selectedIndex === items.length - 1 ? 0 : this.selectedIndex + 1;
    } else if (k === "pageup") {
      this.selectedIndex = Math.max(0, this.selectedIndex - this.maxVisible);
    } else if (k === "pagedown") {
      this.selectedIndex = Math.max(0, Math.min(items.length - 1, this.selectedIndex + this.maxVisible));
    } else if (k === "enter" || ((k === "space" || k === " ") && (!this.searchInput || this.searchInput.getValue() === ""))) {
      this.activate();
    } else if (k === "escape" || k === "ctrl+c") {
      this.onCancel();
    } else if (this.searchInput) {
      this.searchInput.handleInput(key, text);
      const q = this.searchInput.getValue().toLowerCase();
      this.filteredItems = this.items.filter((it) => it.label.toLowerCase().includes(q));
      this.selectedIndex = 0;
    }
  }
  activate() {
    const item = this.displayItems()[this.selectedIndex];
    if (!item) return;
    if (item.submenu) {
      this.submenuItemIndex = this.selectedIndex;
      this.submenuComponent = item.submenu(item.currentValue, (selected, opts) => {
        if (selected !== undefined) {
          item.currentValue = selected;
          this.onChange(item.id, selected);
        }
        this.submenuComponent = null;
        if (opts && opts.navigateTo) {
          this.selectItem(opts.navigateTo);
          this.activate();
        } else if (this.submenuItemIndex !== null) {
          this.selectedIndex = this.submenuItemIndex;
        }
        this.submenuItemIndex = null;
      });
    } else if (item.values && item.values.length > 0) {
      const next = item.values[(item.values.indexOf(item.currentValue) + 1) % item.values.length];
      item.currentValue = next;
      this.onChange(item.id, next);
    }
  }
}

// pi's multi-line Editor: Enter submits (onSubmit(text), unless disableSubmit), Shift+Enter
// inserts a line break, arrows / Home / End move, Backspace / Delete edit. onChange(text) after
// every edit. The constructor takes pi's (tui, theme, options) and ignores them.
export class Editor {
  constructor(tui, theme, options) {
    this.lines = [[]];
    this.row = 0;
    this.col = 0;
    this.onSubmit = undefined;
    this.onChange = undefined;
    this.disableSubmit = false;
    this.borderColor = (theme && theme.borderColor) || ((s) => s);
  }
  getText() {
    return this.lines.map((l) => l.join("")).join("\n");
  }
  getLines() {
    return this.lines.map((l) => l.join(""));
  }
  getCursor() {
    return { line: this.row, col: this.col };
  }
  setText(text) {
    this.lines = String(text).split("\n").map((l) => Array.from(l));
    this.row = this.lines.length - 1;
    this.col = this.lines[this.row].length;
    this.changed();
  }
  insertTextAtCursor(text) {
    const parts = String(text).split("\n");
    const line = this.lines[this.row];
    const tail = line.splice(this.col);
    line.push(...Array.from(parts[0]));
    this.col = line.length;
    for (let i = 1; i < parts.length; i++) {
      this.lines.splice(++this.row, 0, Array.from(parts[i]));
      this.col = this.lines[this.row].length;
    }
    this.lines[this.row].push(...tail);
    this.changed();
  }
  changed() {
    if (this.onChange) this.onChange(this.getText());
  }
  invalidate() {}
  render(width) {
    const border = this.borderColor("─".repeat(Math.max(1, width)));
    const out = [border];
    this.lines.forEach((l, r) => {
      const text = r === this.row ? l.slice(0, this.col).join("") + "|" + l.slice(this.col).join("") : l.join("");
      for (const part of wrapLine(text, Math.max(1, width))) out.push(part);
    });
    out.push(border);
    return out;
  }
  handleInput(key, text) {
    const k = normKey(key);
    const line = this.lines[this.row];
    if (k === "enter") {
      if (!this.disableSubmit && this.onSubmit) this.onSubmit(this.getText());
    } else if (k === "shift+enter" || k === "alt+enter" || k === "ctrl+j") {
      this.insertTextAtCursor("\n");
    } else if (k === "backspace") {
      if (this.col > 0) {
        line.splice(this.col - 1, 1);
        this.col -= 1;
      } else if (this.row > 0) {
        const prev = this.lines[this.row - 1];
        this.col = prev.length;
        prev.push(...line);
        this.lines.splice(this.row, 1);
        this.row -= 1;
      }
      this.changed();
    } else if (k === "delete") {
      if (this.col < line.length) line.splice(this.col, 1);
      else if (this.row + 1 < this.lines.length) line.push(...this.lines.splice(this.row + 1, 1)[0]);
      this.changed();
    } else if (k === "left") {
      if (this.col > 0) this.col -= 1;
      else if (this.row > 0) this.col = this.lines[--this.row].length;
    } else if (k === "right") {
      if (this.col < line.length) this.col += 1;
      else if (this.row + 1 < this.lines.length) {
        this.row += 1;
        this.col = 0;
      }
    } else if (k === "up") {
      if (this.row > 0) this.col = Math.min(this.col, this.lines[--this.row].length);
    } else if (k === "down") {
      if (this.row + 1 < this.lines.length) this.col = Math.min(this.col, this.lines[++this.row].length);
    } else if (k === "home") {
      this.col = 0;
    } else if (k === "end") {
      this.col = line.length;
    } else if (typeof text === "string" && text.length > 0 && !/(^|\+)(ctrl|alt|super)\+/.test(k)) {
      this.insertTextAtCursor(text);
    }
  }
}
