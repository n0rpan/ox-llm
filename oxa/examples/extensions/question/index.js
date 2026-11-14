// ported from pi examples/extensions/question.ts (earendil-works/pi @47e9d74, MIT; see NOTICE)
//
// Question Tool - Single question with options
// Full custom UI: options list + inline editor for "Type something..."
// Escape in editor returns to options, Escape in options cancels
//
// Changes from pi (mechanical): pi -> oxa; the types are removed; pi-tui imports come from
// `oxa:tui`; the TypeBox schema is the JSON Schema it compiles to; oxa delivers keys as
// `handleInput(key, text)`, so the typed text is passed on to the editor; `renderCall` /
// `renderResult` return lines (oxa's renderer shape) instead of a `Text` component, without
// theme colours.

import { Editor, Key, matchesKey, visibleWidth, wrapTextWithAnsi } from "oxa:tui";

export default function question(oxa) {
  oxa.registerTool({
    name: "question",
    label: "Question",
    description: "Ask the user a question and let them pick from options. Use when you need user input to proceed.",
    parameters: {
      type: "object",
      properties: {
        question: { type: "string", description: "The question to ask the user" },
        options: {
          type: "array",
          description: "Options for the user to choose from",
          items: {
            type: "object",
            properties: {
              label: { type: "string", description: "Display label for the option" },
              description: { type: "string", description: "Optional description shown below label" },
            },
            required: ["label"],
          },
        },
      },
      required: ["question", "options"],
    },
    executionMode: "sequential",

    async execute(_toolCallId, params, _signal, _onUpdate, ctx) {
      if (ctx.mode !== "tui") {
        return {
          content: [{ type: "text", text: "Error: UI not available (running in non-interactive mode)" }],
          details: {
            question: params.question,
            options: params.options.map((o) => o.label),
            answer: null,
          },
        };
      }

      if (params.options.length === 0) {
        return {
          content: [{ type: "text", text: "Error: No options provided" }],
          details: { question: params.question, options: [], answer: null },
        };
      }

      const allOptions = [...params.options, { label: "Type something.", isOther: true }];

      const result = await ctx.ui.custom((tui, theme, _kb, done) => {
        let optionIndex = 0;
        let editMode = false;
        let cachedLines;

        const editorTheme = {
          borderColor: (s) => theme.fg("accent", s),
          selectList: {
            selectedPrefix: (t) => theme.fg("accent", t),
            selectedText: (t) => theme.fg("accent", t),
            description: (t) => theme.fg("muted", t),
            scrollInfo: (t) => theme.fg("dim", t),
            noMatch: (t) => theme.fg("warning", t),
          },
        };
        const editor = new Editor(tui, editorTheme);

        editor.onSubmit = (value) => {
          const trimmed = value.trim();
          if (trimmed) {
            done({ answer: trimmed, wasCustom: true });
          } else {
            editMode = false;
            editor.setText("");
            refresh();
          }
        };

        function refresh() {
          cachedLines = undefined;
          tui.requestRender();
        }

        function handleInput(data, text) {
          if (editMode) {
            if (matchesKey(data, Key.escape)) {
              editMode = false;
              editor.setText("");
              refresh();
              return;
            }
            editor.handleInput(data, text);
            refresh();
            return;
          }

          if (matchesKey(data, Key.up)) {
            optionIndex = Math.max(0, optionIndex - 1);
            refresh();
            return;
          }
          if (matchesKey(data, Key.down)) {
            optionIndex = Math.min(allOptions.length - 1, optionIndex + 1);
            refresh();
            return;
          }

          if (matchesKey(data, Key.enter)) {
            const selected = allOptions[optionIndex];
            if (selected.isOther) {
              editMode = true;
              refresh();
            } else {
              done({ answer: selected.label, wasCustom: false, index: optionIndex + 1 });
            }
            return;
          }

          if (matchesKey(data, Key.escape)) {
            done(null);
          }
        }

        function render(width) {
          if (cachedLines) return cachedLines;

          const lines = [];
          const renderWidth = Math.max(1, width);

          function addWrapped(text) {
            lines.push(...wrapTextWithAnsi(text, renderWidth));
          }

          function addWrappedWithPrefix(prefix, text) {
            const prefixWidth = visibleWidth(prefix);
            if (prefixWidth >= renderWidth) {
              addWrapped(prefix + text);
              return;
            }
            const wrapped = wrapTextWithAnsi(text, renderWidth - prefixWidth);
            const continuationPrefix = " ".repeat(prefixWidth);
            for (let i = 0; i < wrapped.length; i++) {
              lines.push(`${i === 0 ? prefix : continuationPrefix}${wrapped[i]}`);
            }
          }

          lines.push(theme.fg("accent", "─".repeat(renderWidth)));
          addWrappedWithPrefix(" ", theme.fg("text", params.question));
          lines.push("");

          for (let i = 0; i < allOptions.length; i++) {
            const opt = allOptions[i];
            const selected = i === optionIndex;
            const isOther = opt.isOther === true;
            const prefix = selected ? theme.fg("accent", "> ") : "  ";
            const label = `${i + 1}. ${opt.label}${isOther && editMode ? " ✎" : ""}`;
            const color = selected || (isOther && editMode) ? "accent" : "text";

            addWrappedWithPrefix(prefix, theme.fg(color, label));

            // Show description if present
            if (opt.description) {
              addWrappedWithPrefix("     ", theme.fg("muted", opt.description));
            }
          }

          if (editMode) {
            lines.push("");
            addWrappedWithPrefix(" ", theme.fg("muted", "Your answer:"));
            for (const line of editor.render(Math.max(1, renderWidth - 2))) {
              lines.push(` ${line}`);
            }
          }

          lines.push("");
          if (editMode) {
            addWrappedWithPrefix(" ", theme.fg("dim", "Enter to submit • Esc to go back"));
          } else {
            addWrappedWithPrefix(" ", theme.fg("dim", "↑↓ navigate • Enter to select • Esc to cancel"));
          }
          lines.push(theme.fg("accent", "─".repeat(renderWidth)));

          cachedLines = lines;
          return lines;
        }

        return {
          render,
          invalidate: () => {
            cachedLines = undefined;
          },
          handleInput,
        };
      });

      // Build simple options list for details
      const simpleOptions = params.options.map((o) => o.label);

      if (!result) {
        return {
          content: [{ type: "text", text: "User cancelled the selection" }],
          details: { question: params.question, options: simpleOptions, answer: null },
        };
      }

      if (result.wasCustom) {
        return {
          content: [{ type: "text", text: `User wrote: ${result.answer}` }],
          details: {
            question: params.question,
            options: simpleOptions,
            answer: result.answer,
            wasCustom: true,
          },
        };
      }
      return {
        content: [{ type: "text", text: `User selected: ${result.index}. ${result.answer}` }],
        details: {
          question: params.question,
          options: simpleOptions,
          answer: result.answer,
          wasCustom: false,
        },
      };
    },

    renderCall(args) {
      const lines = [`question ${args.question ?? ""}`];
      const opts = Array.isArray(args.options) ? args.options : [];
      if (opts.length) {
        const numbered = [...opts.map((o) => o.label), "Type something."].map((o, i) => `${i + 1}. ${o}`);
        lines.push(`  Options: ${numbered.join(", ")}`);
      }
      return lines;
    },

    renderResult(result) {
      const details = result && result.details;
      if (!details) {
        const first = result && result.content && result.content[0];
        return [first && first.type === "text" ? first.text : ""];
      }
      if (details.answer === null || details.answer === undefined) {
        return ["Cancelled"];
      }
      if (details.wasCustom) {
        return [`(wrote) ${details.answer}`];
      }
      const idx = details.options.indexOf(details.answer) + 1;
      return [idx > 0 ? `${idx}. ${details.answer}` : details.answer];
    },
  });
}
