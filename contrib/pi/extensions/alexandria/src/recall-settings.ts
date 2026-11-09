import type { RecallSettingKey, RecallSettingsSave, RecallSettingsStore, RecallSettingsView } from "./recall-config.js";

interface SettingsUi {
	select(title: string, options: string[]): Promise<string | undefined>;
	input(title: string, placeholder?: string): Promise<string | undefined>;
	notify(message: string, type?: "info" | "warning" | "error"): void;
}

function option(label: string, key: RecallSettingKey, view: RecallSettingsView): string {
	const display = (value: boolean | number) =>
		typeof value === "boolean" ? (value ? "on" : "off") : String(value);
	const effective = display(view.effective[key]);
	const override = view.overrides[key];
	return override
		? `${label}: ${effective} (${override}; saved ${display(view.saved[key])})`
		: `${label}: ${effective}`;
}

export async function showRecallSettings(
	ui: SettingsUi,
	store: RecallSettingsStore,
	prewarm: () => void,
): Promise<void> {
	for (;;) {
		const view = store.get();
		const choices = [
			option("Auto-recall", "enabled", view),
			option("Result limit", "limit", view),
			option("Minimum similarity", "minSimilarity", view),
			"Done",
		];
		const selection = await ui.select("Alexandria auto-recall", choices);
		const index = choices.indexOf(selection ?? "");
		if (index < 0 || index === 3) return;

		const key = (["enabled", "limit", "minSimilarity"] as const)[index];
		let value: boolean | number;
		if (key === "enabled") {
			value = !view.saved.enabled;
		} else {
			const title = key === "limit" ? "Result limit (1–100)" : "Minimum similarity (-1 to 1)";
			const entered = await ui.input(title, String(view.saved[key]));
			if (entered === undefined) continue;
			value = entered.trim() === "" ? Number.NaN : Number(entered);
		}
		let updated: RecallSettingsSave;
		try {
			updated = key === "enabled"
				? store.save("enabled", value as boolean)
				: store.save(key, value as number);
		} catch (error) {
			ui.notify(`Alexandria settings were not saved: ${error instanceof Error ? error.message : String(error)}`, "error");
			continue;
		}
		if (updated.cleanupWarning) ui.notify(updated.cleanupWarning, "warning");
		if (!view.effective.enabled && updated.effective.enabled) {
			try { prewarm(); } catch (error) {
				ui.notify(`Settings saved, but prewarm failed: ${error instanceof Error ? error.message : String(error)}`, "warning");
			}
		}
		if (updated.overrides[key]) {
			ui.notify(`Saved ${key}; ${updated.overrides[key]} still controls the effective value.`, "warning");
		} else {
			ui.notify(`Saved ${key}.`, "info");
		}
	}
}
