import { useMemo } from "react";
import { AlertTriangle } from "lucide-react";
import { useOutletContext } from "react-router-dom";
import { useStore } from "@/store";
import { useIndexMeta, useRecentPrompts, useStats } from "@/hooks/queries";
import type { LayoutOutletContext } from "@/components/Layout";
import { StatsOverview } from "@/components/StatsOverview";
import { ActivityChart, HourChart, ProjectChart } from "@/components/Charts";
import { TokenStats } from "@/components/TokenStats";
import { PromptList } from "@/components/PromptList";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
  CenterMessage,
  Skeleton,
} from "@/components/ui";
import { errMessage } from "@/lib/api";
import { useT } from "@/i18n";
import { absoluteTime, cn, formatNumber } from "@/lib/utils";

function StatsSkeleton() {
  return (
    <div className="grid grid-cols-2 gap-3 md:grid-cols-3 xl:grid-cols-6">
      {Array.from({ length: 6 }).map((_, i) => (
        <Skeleton key={i} className="h-[88px] w-full" />
      ))}
    </div>
  );
}

function ListSkeleton() {
  return (
    <div className="space-y-2.5">
      {Array.from({ length: 5 }).map((_, i) => (
        <Skeleton key={i} className="h-20 w-full" />
      ))}
    </div>
  );
}

function PanelSkeleton({ height }: { height: string }) {
  return (
    <Card>
      <CardHeader>
        <Skeleton className="h-4 w-28" />
      </CardHeader>
      <CardContent>
        <Skeleton className={height} />
      </CardContent>
    </Card>
  );
}

function HomeRefreshingSkeleton() {
  return (
    <div className="home-refresh-frame mx-auto max-w-5xl space-y-6 px-6 py-6">
      <div className="space-y-2">
        <Skeleton className="h-7 w-20" />
        <Skeleton className="h-4 w-72 max-w-full" />
      </div>

      <StatsSkeleton />

      <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
        <PanelSkeleton height="h-[220px] w-full" />
        <PanelSkeleton height="h-[220px] w-full" />
      </div>

      <PanelSkeleton height="h-[260px] w-full" />
      <PanelSkeleton height="h-[360px] w-full" />

      <div>
        <Skeleton className="mb-3 h-4 w-24" />
        <ListSkeleton />
      </div>
    </div>
  );
}

export function Home() {
  const { includeCommands } = useStore();
  const { refreshing } = useOutletContext<LayoutOutletContext>();
  const t = useT();
  const statsQ = useStats();
  const metaQ = useIndexMeta();
  const recentQ = useRecentPrompts(24, includeCommands);

  // memo 保持引用稳定：PromptList 以 items 引用变化作为重置分批的信号
  const recentItems = useMemo(
    () => (recentQ.data ?? []).map((entry) => ({ entry })),
    [recentQ.data]
  );

  return (
    <div className="relative">
      <div
        className={cn(
          "transition-all duration-200 ease-out motion-reduce:transition-none",
          refreshing &&
            "pointer-events-none translate-y-1 opacity-0 motion-reduce:translate-y-0"
        )}
      >
        <div className="mx-auto max-w-5xl space-y-6 px-6 py-6">
          <div>
            <h1 className="text-xl font-semibold text-foreground">
              {t("overviewTitle")}
            </h1>
            <p className="mt-0.5 text-xs text-muted">
              {metaQ.data
                ? [
                    t("indexMetaSummary", {
                      files: formatNumber(metaQ.data.sourceFiles),
                      time: absoluteTime(metaQ.data.builtAt),
                    }),
                    metaQ.data.fromCache
                      ? t("indexFromCache")
                      : t("indexFreshScan"),
                    ...(metaQ.data.reparsedFiles > 0
                      ? [
                          t("indexReparsedFiles", {
                            count: formatNumber(metaQ.data.reparsedFiles),
                          }),
                        ]
                      : []),
                  ].join(" · ")
                : t("loadingLocalData")}
            </p>
          </div>

          {statsQ.isLoading ? (
            <StatsSkeleton />
          ) : statsQ.isError ? (
            <CenterMessage
              icon={<AlertTriangle size={28} />}
              title={t("cannotLoadData")}
              hint={t("cannotLoadDataHint", { error: errMessage(statsQ.error) })}
            />
          ) : statsQ.data ? (
            <>
              <StatsOverview stats={statsQ.data} />

              <div className="grid grid-cols-1 gap-3 lg:grid-cols-2">
                <Card>
                  <CardHeader>
                    <CardTitle>{t("dailyActivity")}</CardTitle>
                  </CardHeader>
                  <CardContent>
                    <ActivityChart data={statsQ.data.byDay} />
                  </CardContent>
                </Card>
                <Card>
                  <CardHeader>
                    <CardTitle>{t("hourlyDistribution")}</CardTitle>
                  </CardHeader>
                  <CardContent>
                    <HourChart data={statsQ.data.byHour} />
                  </CardContent>
                </Card>
              </div>

              <Card>
                <CardHeader>
                  <CardTitle>{t("topActiveFolders")}</CardTitle>
                </CardHeader>
                <CardContent>
                  <ProjectChart data={statsQ.data.topProjects} />
                </CardContent>
              </Card>

              <TokenStats usage={statsQ.data.usage} />
            </>
          ) : null}

          <div>
            <h2 className="mb-3 text-sm font-semibold text-foreground">
              {t("recentPrompts")}
            </h2>
            {recentQ.isLoading ? (
              <ListSkeleton />
            ) : recentQ.isError ? (
              <p className="text-xs text-muted">
                {t("loadFailedWithError", { error: errMessage(recentQ.error) })}
              </p>
            ) : recentItems.length > 0 ? (
              <PromptList items={recentItems} showProject />
            ) : (
              <p className="text-xs text-muted">{t("noPromptRecords")}</p>
            )}
          </div>
        </div>
      </div>
      {refreshing && (
        <div className="pointer-events-none absolute inset-x-0 top-0">
          <HomeRefreshingSkeleton />
        </div>
      )}
    </div>
  );
}
