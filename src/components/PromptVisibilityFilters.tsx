import type { PromptVisibility } from "@/lib/types";
import { useStore } from "@/store";
import { useT, type DictKey } from "@/i18n";
import { cn } from "@/lib/utils";

const filterItems: Array<{
  key: Exclude<keyof PromptVisibility, "includeCommands">;
  labelKey: DictKey;
}> = [
  { key: "includeMeta", labelKey: "metaBadge" },
  { key: "includeSidechain", labelKey: "sidechainBadge" },
  { key: "includeSystem", labelKey: "systemBadge" },
  { key: "includeQueued", labelKey: "queuedBadge" },
  { key: "includeSdk", labelKey: "sdkBadge" },
  { key: "includeOther", labelKey: "otherBadge" },
];

export function PromptVisibilityFilters({
  className,
}: {
  className?: string;
}) {
  const t = useT();
  const { promptVisibility, setPromptVisibility } = useStore();

  return (
    <div
      className={cn(
        "flex flex-wrap items-center gap-x-3 gap-y-2 rounded-lg border border-border bg-surface px-3 py-2 text-[11px] text-muted",
        className
      )}
    >
      <span className="font-semibold uppercase tracking-wide text-muted">
        {t("otherKindsLabel")}
      </span>
      {filterItems.map((item) => (
        <label
          key={item.key}
          className="flex items-center gap-1.5 text-foreground"
        >
          <input
            type="checkbox"
            checked={promptVisibility[item.key]}
            onChange={(e) => setPromptVisibility(item.key, e.target.checked)}
            className="h-3.5 w-3.5 accent-accent"
          />
          <span>{t(item.labelKey)}</span>
        </label>
      ))}
    </div>
  );
}
