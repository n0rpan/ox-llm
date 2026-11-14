// ported from pi examples/extensions/tools.ts (earendil-works/pi @47e9d74, MIT; see NOTICE)
//
// Tools extension: a /tools command to enable or disable tools. The selection persists in the
// session (`appendEntry("tools-config")`) and follows branch navigation.
//
// Changes from pi (mechanical): pi -> oxa; the types are removed; pi-tui imports come from
// `oxa:tui`; `getSettingsListTheme()` (a pi-coding-agent export) is left out, so the list uses
// the shim's plain theme; oxa delivers keys as `handleInput(key, text)`, which the component
// passes on to the list.

import { Container, SettingsList } from "oxa:tui";

export default function toolsExtension(oxa) {
  // Tool names enabled in the current session branch.
  let enabledTools = new Set();
  let allTools = [];

  // Persist the current selection as a session entry.
  function persistState() {
    oxa.appendEntry("tools-config", { enabledTools: Array.from(enabledTools) });
  }

  // Apply the current selection to the active tools.
  function applyTools() {
    oxa.setActiveTools(Array.from(enabledTools));
  }

  // Restore from the last tools-config entry of the current branch.
  function restoreFromBranch(ctx) {
    allTools = oxa.getAllTools();

    const branchEntries = ctx.sessionManager.getBranch();
    let savedTools;
    for (const entry of branchEntries) {
      if (entry.type === "custom" && entry.customType === "tools-config") {
        const data = entry.data;
        if (data && Array.isArray(data.enabledTools)) {
          savedTools = data.enabledTools;
        }
      }
    }

    if (savedTools) {
      // Keep only the saved tools that still exist.
      const allToolNames = allTools.map((t) => t.name);
      enabledTools = new Set(savedTools.filter((t) => allToolNames.includes(t)));
      applyTools();
    } else {
      // No saved state: sync with the currently active tools.
      enabledTools = new Set(oxa.getActiveTools());
    }
  }

  oxa.registerCommand("tools", {
    description: "Enable/disable tools",
    handler: async (_args, ctx) => {
      if (ctx.mode !== "tui") {
        ctx.ui.notify("/tools requires TUI mode", "error");
        return;
      }

      // Refresh the tool list.
      allTools = oxa.getAllTools();

      await ctx.ui.custom((tui, theme, _kb, done) => {
        // Build settings items for each tool
        const items = allTools.map((tool) => ({
          id: tool.name,
          label: tool.name,
          currentValue: enabledTools.has(tool.name) ? "enabled" : "disabled",
          values: ["enabled", "disabled"],
        }));

        const container = new Container();
        container.addChild(
          new (class {
            render(_width) {
              return [theme.fg("accent", theme.bold("Tool Configuration")), ""];
            }
            invalidate() {}
          })(),
        );

        const settingsList = new SettingsList(
          items,
          Math.min(items.length + 2, 15),
          undefined,
          (id, newValue) => {
            // Update enabled state and apply immediately
            if (newValue === "enabled") {
              enabledTools.add(id);
            } else {
              enabledTools.delete(id);
            }
            applyTools();
            persistState();
          },
          () => {
            // Close dialog
            done(undefined);
          },
        );

        container.addChild(settingsList);

        const component = {
          // Let PageUp/PageDown page the list instead of scrolling the fullscreen transcript.
          capturesViewportInput: settingsList.capturesViewportInput,
          render(width) {
            return container.render(width);
          },
          invalidate() {
            container.invalidate();
          },
          handleInput(data, text) {
            settingsList.handleInput?.(data, text);
            tui.requestRender();
          },
        };

        return component;
      });
    },
  });

  // Restore the selection on session start and when navigating the session tree.
  oxa.on("session_start", (_event, ctx) => {
    restoreFromBranch(ctx);
  });

  oxa.on("session_tree", (_event, ctx) => {
    restoreFromBranch(ctx);
  });
}
