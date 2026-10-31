import {
  lazy,
  Suspense,
  type ComponentType,
  type ReactNode,
} from "react";
import {
  resolveDynamicExport,
  type DynamicComponent,
} from "./dynamic-export";

type DynamicOptions = {
  ssr?: boolean;
  loading?: ComponentType;
};

export default function dynamic(
  loader: () => Promise<unknown>,
  options: DynamicOptions = {},
) {
  const LazyComponent = lazy(async () => {
    const component: DynamicComponent = resolveDynamicExport(await loader());
    return { default: component };
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
