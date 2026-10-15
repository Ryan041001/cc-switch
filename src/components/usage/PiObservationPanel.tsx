import { Activity } from "lucide-react";
import { useTranslation } from "react-i18next";
import { PiObservationControl } from "./PiObservationControl";

/** Observation is independent of the selected provider and native config. */
export function PiObservationPanel() {
  const { t } = useTranslation();
  return (
    <section
      aria-label={t("usage.sources.piMetricsTitle")}
      className="rounded-xl border border-border bg-bg-2 p-4 space-y-2"
    >
      <h3 className="m-0 flex items-center gap-2 text-sm font-medium">
        <Activity size={16} aria-hidden="true" />
        {t("usage.sources.piMetricsTitle")}
      </h3>
      <p className="m-0 text-caption text-fg-2">
        {t("usage.sources.piMetricsNativeConfig")}
      </p>
      <PiObservationControl compact />
    </section>
  );
}
