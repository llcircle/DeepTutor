import {
  Component,
  type ComponentType,
  type ErrorInfo,
  lazy,
  StrictMode,
  Suspense,
} from "react";
import { createRoot } from "react-dom/client";
import {
  BrowserRouter,
  Navigate,
  Route,
  Routes,
  useLocation,
  useParams as useRouterParams,
} from "react-router-dom";

import "@/app/globals.css";
import "@/features/ui-layout/classic/classic.css";
import "@/features/ui-layout/modern/modern.css";

import MotionProvider from "@/components/common/MotionProvider";
import ToastViewport from "@/components/common/ToastViewport";
import SettingsReturnTracker from "@/components/settings/SettingsReturnTracker";
import { WorkspaceNavigation } from "@/components/workspaces/WorkspaceNavigation";
import { AppShellProvider } from "@/context/AppShellContext";
import { I18nClientBridge } from "@/i18n/I18nClientBridge";

type RouteModule = { default: ComponentType<Record<string, unknown>> };
type ModuleLoader = () => Promise<RouteModule>;
type ModuleMap = Record<string, ModuleLoader>;

const pageModules = import.meta.glob<RouteModule>("../app/**/page.tsx");
const layoutModules = import.meta.glob<RouteModule>("../app/**/layout.tsx");

const ROOT_GROUP_PAGE = "../app/(workspace)/page.tsx";
const SETTINGS_SECTION_PAGE = "../app/(settings)/settings/[section]/page.tsx";
const READING_FOLDER_PAGE =
  "../app/(workspace)/learning/reading/folders/[folderId]/page.tsx";
const LEGACY_WATCHING_SESSION_PAGE =
  "../app/(workspace)/learning/watching/[sessionId]/page.tsx";
const LEGACY_WATCHING_HOME_PAGE =
  "../app/(workspace)/learning/watching/page.tsx";

function pathSegments(key: string, suffix: string): string[] {
  return key
    .replace(/^\.\.\/app\//, "")
    .slice(0, -suffix.length)
    .split("/")
    .filter(Boolean);
}

function routePath(key: string): string {
  const segments = pathSegments(key, "/page.tsx")
    .filter((segment) => !/^\(.+\)$/.test(segment))
    .map((segment) => {
      const dynamic = segment.match(/^\[([^\]]+)\]$/);
      return dynamic ? `:${dynamic[1]}` : segment;
    });
  return segments.length ? `/${segments.join("/")}` : "/";
}

function layoutKeysFor(pageKey: string): string[] {
  const page = pathSegments(pageKey, "/page.tsx");
  return Object.keys(layoutModules)
    .filter((layoutKey) => layoutKey !== "../app/layout.tsx")
    .filter((layoutKey) => {
      const layout = pathSegments(layoutKey, "/layout.tsx");
      return (
        layout.length <= page.length &&
        layout.every((segment, index) => segment === page[index])
      );
    })
    .sort(
      (left, right) =>
        pathSegments(left, "/layout.tsx").length -
        pathSegments(right, "/layout.tsx").length,
    );
}

function composeRoute(pageKey: string, pageLoader: ModuleLoader) {
  const layouts = layoutKeysFor(pageKey).map((key) => layoutModules[key]);
  return lazy(async () => {
    const [pageModule, ...layoutBundles] = await Promise.all([
      pageLoader(),
      ...layouts.map((load) => load()),
    ]);
    const Page = pageModule.default;
    return {
      default: function LoadedDesktopRoute() {
        let content = <Page />;
        for (const bundle of [...layoutBundles].reverse()) {
          const Layout = bundle.default;
          content = <Layout>{content}</Layout>;
        }
        return content;
      },
    };
  });
}

function SettingsSectionRoute() {
  const { section = "general" } = useRouterParams();
  const Content = lazy(async () => {
    const importedModule = await import("@/components/settings/SettingsPageContent");
    return {
      default: () => (
        <importedModule.default section={decodeURIComponent(section)} />
      ),
    };
  });
  return <Content />;
}

function ReadingFolderRoute() {
  const { folderId = "" } = useRouterParams();
  const Content = lazy(async () => {
    const importedModule = await import("@/components/reading/library/ReadingFolder");
    return {
      default: () => (
        <importedModule.ReadingFolderPage
          folderId={decodeURIComponent(folderId)}
        />
      ),
    };
  });
  return <Content />;
}

function WatchingSessionRoute() {
  const { sessionId = "" } = useRouterParams();
  const Content = lazy(
    () => import("@/components/reading/workspace/LegacyWatchingSession"),
  );
  return <Content sessionId={decodeURIComponent(sessionId)} />;
}

function WatchingHomeRedirect() {
  const location = useLocation();
  return (
    <Navigate
      replace
      to={`/learning/reading${location.search}${location.hash}`}
    />
  );
}

function LegacyRemainderRedirect({ prefix }: { prefix: string }) {
  const location = useLocation();
  const params = useRouterParams();
  const remainder = params["*"] ?? "";
  return (
    <Navigate
      replace
      to={`${prefix}${remainder ? `/${remainder}` : ""}${location.search}${location.hash}`}
    />
  );
}

class DesktopErrorBoundary extends Component<
  { children: React.ReactNode },
  { error: Error | null }
> {
  state = { error: null as Error | null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("DeepTutor 页面渲染失败", error, info);
  }

  render() {
    if (this.state.error) {
      return (
        <main className="flex min-h-screen items-center justify-center p-8">
          <section className="w-full max-w-xl rounded-2xl border border-[var(--border)] bg-[var(--card)] p-7 shadow-lg">
            <h1 className="text-xl font-semibold">页面加载失败</h1>
            <p className="mt-3 whitespace-pre-wrap text-sm text-[var(--muted-foreground)]">
              {this.state.error.message}
            </p>
            <button
              className="mt-5 rounded-lg bg-[var(--primary)] px-4 py-2 text-sm text-[var(--primary-foreground)]"
              onClick={() => window.location.reload()}
              type="button"
            >
              重新加载
            </button>
          </section>
        </main>
      );
    }
    return this.props.children;
  }
}

function RouteLoading() {
  return (
    <div
      aria-label="正在加载页面"
      className="flex min-h-[45vh] items-center justify-center"
    >
      <div className="h-8 w-8 animate-spin rounded-full border-2 border-[var(--border)] border-t-[var(--primary)]" />
    </div>
  );
}

const specialPages: Record<string, ComponentType> = {
  [SETTINGS_SECTION_PAGE]: SettingsSectionRoute,
  [READING_FOLDER_PAGE]: ReadingFolderRoute,
  [LEGACY_WATCHING_SESSION_PAGE]: WatchingSessionRoute,
  [LEGACY_WATCHING_HOME_PAGE]: WatchingHomeRedirect,
};

const routes = Object.entries(pageModules)
  .filter(([key]) => key !== ROOT_GROUP_PAGE)
  .map(([key, loader]) => ({
    key,
    path: routePath(key),
    Component: specialPages[key] ?? composeRoute(key, loader),
  }));

routes.push({
  key: ROOT_GROUP_PAGE,
  path: "/",
  Component: () => <Navigate replace to="/chat" />,
});

function DesktopApp() {
  return (
    <BrowserRouter>
      <DesktopErrorBoundary>
        <AppShellProvider>
          <SettingsReturnTracker />
          <WorkspaceNavigation />
          <MotionProvider>
            <I18nClientBridge>
              <Suspense fallback={<RouteLoading />}>
                <Routes>
                  <Route
                    path="/notebook/*"
                    element={<LegacyRemainderRedirect prefix="/notebooks" />}
                  />
                  <Route
                    path="/mastery/:pathId/study"
                    element={
                      <LegacyMasteryRedirect suffix="/sessions" />
                    }
                  />
                  <Route
                    path="/mastery/:pathId/study/:sessionId"
                    element={
                      <LegacyMasteryRedirect suffix="/sessions" />
                    }
                  />
                  <Route
                    path="/books/*"
                    element={<LegacyRemainderRedirect prefix="/learning/books" />}
                  />
                  <Route
                    path="/mastery/*"
                    element={<LegacyRemainderRedirect prefix="/learning/mastery" />}
                  />
                  <Route
                    path="/reading/*"
                    element={<LegacyRemainderRedirect prefix="/learning/reading" />}
                  />
                  <Route
                    path="/watching/*"
                    element={<LegacyRemainderRedirect prefix="/learning/watching" />}
                  />
                  {routes.map(({ key, path, Component }) => (
                    <Route key={key} path={path} element={<Component />} />
                  ))}
                  <Route path="*" element={<NotFound />} />
                </Routes>
              </Suspense>
            </I18nClientBridge>
          </MotionProvider>
          <ToastViewport />
        </AppShellProvider>
      </DesktopErrorBoundary>
    </BrowserRouter>
  );
}

function LegacyMasteryRedirect({ suffix }: { suffix: string }) {
  const location = useLocation();
  const { pathId = "", sessionId } = useRouterParams();
  const target = `/learning/mastery/${encodeURIComponent(pathId)}${suffix}${
    sessionId ? `/${encodeURIComponent(sessionId)}` : ""
  }${location.search}${location.hash}`;
  return <Navigate replace to={target} />;
}

function NotFound() {
  return (
    <main className="flex min-h-[70vh] flex-col items-center justify-center gap-4 p-8 text-center">
      <p className="text-sm font-medium text-[var(--muted-foreground)]">404</p>
      <h1 className="text-2xl font-semibold">找不到这个页面</h1>
      <a
        className="rounded-lg bg-[var(--primary)] px-4 py-2 text-sm text-[var(--primary-foreground)]"
        href="/chat"
      >
        返回学习工作台
      </a>
    </main>
  );
}

const root = document.getElementById("root");
if (!root) throw new Error("缺少应用挂载节点 #root");

createRoot(root).render(
  <StrictMode>
    <DesktopApp />
  </StrictMode>,
);
