import { useId } from "react";
import { Button } from "@/components/ui/button";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Loader2 } from "lucide-react";
import { Switch } from "@/components/ui/switch";
import { piApi } from "@/lib/api/pi";

const QUERY_KEY = ["pi-observation-status"];

/** Disk state and last imported measurement; neither claims a live heartbeat. */
export function PiObservationControl({
  active = true,
  compact = false,
}: {
  active?: boolean;
  compact?: boolean;
}) {
  const { t, i18n } = useTranslation();
  const id = useId();
  const queryClient = useQueryClient();
  const status = useQuery({
    queryKey: QUERY_KEY,
    queryFn: piApi.getObservationStatus,
    enabled: active,
    refetchInterval: active ? 15_000 : false,
  });
  const mutation = useMutation({
    mutationFn: (enabled: boolean) => piApi.setObservationEnabled(enabled),
    onSuccess: (next) => queryClient.setQueryData(QUERY_KEY, next),
  });
  const error = mutation.error ?? status.error;

  return (
    <div className="flex flex-col gap-2">
      <div className="flex items-center justify-between gap-3">
        <label htmlFor={id} className="text-body text-fg-1">
          {t("usage.sources.piMetricsInstall")}
        </label>
        {status.isLoading ? (
          <Loader2 className="h-4 w-4 animate-spin text-fg-3" />
        ) : (
          <Switch
            id={id}
            aria-label={t("usage.sources.piMetricsInstall")}
            checked={status.data?.enabled ?? false}
            disabled={!active || !status.data?.canManage || mutation.isPending}
            onCheckedChange={(value) => mutation.mutate(value)}
          />
        )}
      </div>
      <p className="m-0 text-caption text-fg-2">
        {t("usage.sources.piMetricsReload")}
      </p>
      {status.data?.updateAvailable && (
        <Button
          size="sm"
          variant="outline"
          disabled={!active || !status.data.canManage || mutation.isPending}
          onClick={() => mutation.mutate(true)}
        >
          {t("usage.sources.piMetricsUpgrade")}
        </Button>
      )}
      {status.data?.enabled && (
        <p className="m-0 text-caption text-fg-3" role="status">
          {status.data.lastObservedAt
            ? t("usage.sources.piMetricsLastObserved", {
                time: new Date(
                  status.data.lastObservedAt * 1000,
                ).toLocaleString(i18n.resolvedLanguage ?? i18n.language),
              })
            : t("usage.sources.piMetricsAwaiting")}
        </p>
      )}
      {status.data && !status.data.canManage && (
        <p role="alert" className="m-0 text-caption text-danger-text">
          {t("usage.sources.piMetricsConflict")}
        </p>
      )}
      {error != null && (
        <p role="alert" className="m-0 text-caption text-danger-text">
          {String(error)}
        </p>
      )}
      {status.data && !compact && (
        <p className="m-0 break-all font-mono text-caption text-fg-3">
          {status.data.extensionPath}
        </p>
      )}
    </div>
  );
}
