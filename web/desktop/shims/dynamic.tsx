import {
  lazy,
  Suspense,
  type ComponentType,
  type ReactNode,
} from "react";

type DynamicOptions = {
  ssr?: boolean;
  loading?: ComponentType;
};

type ModuleShape = {
  default?: ComponentType<Record<string, unknown>>;
  [key: string]: unknown;
};

export default function dynamic(
  loader: () => Promise<ModuleShape>,
  options: DynamicOptions = {},
) {
  const LazyComponent = lazy(async () => {
    const loadedModule = await loader();
    if (loadedModule.default) {
      return loadedModule as { default: ComponentType };
    }
    const firstComponent = Object.values(loadedModule).find(
      (value): value is ComponentType =>
        typeof value === "function" || typeof value === "object",
    );
    if (!firstComponent) {
      throw new Error("动态模块没有可渲染的 React 组件");
    }
    return { default: firstComponent };
  });
  const Loading = options.loading;

  return function DynamicComponent(props: Record<string, unknown>) {
    return (
      <Suspense fallback={Loading ? <Loading /> : <DynamicFallback />}>
        <LazyComponent {...props} />
      </Suspense>
    );
  };
}

function DynamicFallback(): ReactNode {
  return (
    <div
      aria-label="正在加载组件"
      className="flex min-h-24 items-center justify-center"
    >
      <div className="h-6 w-6 animate-spin rounded-full border-2 border-[var(--border)] border-t-[var(--primary)]" />
    </div>
  );
}
