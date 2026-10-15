import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Loader2 } from "lucide-react";
import { Switch } from "@/components/ui/switch";
import { piApi } from "@/lib/api/pi";

const QUERY_KEY = ["pi-observation-status"];

/** Installation state only: a running Pi must /reload after either change. */
export function PiObservationControl({ active }: { active: boolean }) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const status = useQuery({
    queryKey: QUERY_KEY,
    queryFn: piApi.getObservationStatus,
    enabled: active,
  });
  const mutation = useMutation({
    mutationFn: (enabled: boolean) => piApi.setObservationEnabled(enabled),
    onSuccess: (next) => queryClient.setQueryData(QUERY_KEY, next),
  });
  const error = mutation.error ?? status.error;

  return (
    <div className="flex flex-col gap-2">
      <div className="flex items-center justify-between gap-3">
        <label htmlFor="pi-observation" className="text-body text-fg-1">
          {t("usage.sources.piMetricsInstall")}
        </label>
        {status.isLoading ? (
          <Loader2 className="h-4 w-4 animate-spin text-fg-3" />
        ) : (
          <Switch
            id="pi-observation"
            aria-label={t("usage.sources.piMetricsInstall")}
            checked={status.data?.enabled ?? false}
            disabled={!status.data?.canManage || mutation.isPending}
            onCheckedChange={(value) => mutation.mutate(value)}
          />
        )}
      </div>
      <p className="m-0 text-caption text-fg-2">
        {t("usage.sources.piMetricsReload")}
      </p>
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
      {status.data && (
        <p className="m-0 break-all font-mono text-caption text-fg-3">
          {status.data.extensionPath}
        </p>
      )}
    </div>
  );
}
