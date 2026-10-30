import { forwardRef, type AnchorHTMLAttributes, type MouseEvent } from "react";
import { Link as RouterLink } from "react-router-dom";

type NextLinkProps = Omit<AnchorHTMLAttributes<HTMLAnchorElement>, "href"> & {
  href: string | { pathname?: string; query?: string; hash?: string };
  prefetch?: boolean;
  replace?: boolean;
  scroll?: boolean;
  shallow?: boolean;
  legacyBehavior?: boolean;
  passHref?: boolean;
  locale?: string | false;
};

function hrefString(href: NextLinkProps["href"]): string {
  if (typeof href === "string") return href;
  return `${href.pathname ?? "/"}${href.query ?? ""}${href.hash ?? ""}`;
}

const Link = forwardRef<HTMLAnchorElement, NextLinkProps>(function Link(
  {
    href,
    prefetch: _prefetch,
    replace,
    scroll: _scroll,
    shallow: _shallow,
    legacyBehavior: _legacyBehavior,
    passHref: _passHref,
    locale: _locale,
    onClick,
    target,
    ...props
  },
  ref,
) {
  const destination = hrefString(href);
  if (/^(?:[a-z]+:)?\/\//i.test(destination)) {
    return (
      <a
        {...props}
        ref={ref}
        href={destination}
        target={target}
        rel={target === "_blank" ? "noopener noreferrer" : props.rel}
        onClick={onClick}
      />
    );
  }
  return (
    <RouterLink
      {...props}
      ref={ref}
      to={destination}
      replace={replace}
      target={target}
      onClick={onClick as (event: MouseEvent<HTMLAnchorElement>) => void}
    />
  );
});

export default Link;
