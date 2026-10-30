import { useCallback, useEffect, useMemo, useState } from "react";
import {
  useLocation,
  useNavigate,
  useParams as useRouterParams,
} from "react-router-dom";

export function usePathname(): string {
  return useLocation().pathname;
}

export function useSearchParams(): URLSearchParams {
  const { search } = useLocation();
  return useMemo(() => new URLSearchParams(search), [search]);
}

export function useParams<
  T extends Record<string, string | string[] | undefined> = Record<
    string,
    string | string[] | undefined
  >,
>(): T {
  return useRouterParams() as T;
}

export function useRouter() {
  const navigate = useNavigate();
  const location = useLocation();
  return useMemo(
    () => ({
      push(href: string, options?: { scroll?: boolean }) {
        navigate(href, { preventScrollReset: options?.scroll === false });
      },
      replace(href: string, options?: { scroll?: boolean }) {
        navigate(href, {
          replace: true,
          preventScrollReset: options?.scroll === false,
        });
      },
      refresh() {
        window.location.reload();
      },
      back() {
        navigate(-1);
      },
      forward() {
        navigate(1);
      },
      prefetch() {
        // 桌面应用由 Rust 从本地磁盘读取资源，预取没有网络收益。
      },
      pathname: location.pathname,
      search: location.search,
    }),
    [location.pathname, location.search, navigate],
  );
}

export function redirect(href: string): never {
  if (typeof window !== "undefined") window.location.replace(href);
  throw new Error(`Redirected to ${href}`);
}

export function permanentRedirect(href: string): never {
  return redirect(href);
}

export function notFound(): never {
  throw new Error("找不到请求的页面");
}

export function useSelectedLayoutSegment(): string | null {
  const segments = usePathname().split("/").filter(Boolean);
  return segments.at(-1) ?? null;
}

export function useSelectedLayoutSegments(): string[] {
  return usePathname().split("/").filter(Boolean);
}

export function useRouterParamsForDesktop() {
  const [params, setParams] = useState(() =>
    typeof window === "undefined"
      ? ""
      : new URLSearchParams(window.location.search).toString(),
  );
  useEffect(() => {
    const update = () => setParams(window.location.search);
    window.addEventListener("popstate", update);
    return () => window.removeEventListener("popstate", update);
  }, []);
  return useCallback(() => params, [params])();
}
