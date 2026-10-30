import {
  forwardRef,
  type CSSProperties,
  type ImgHTMLAttributes,
} from "react";

type NextImageProps = Omit<
  ImgHTMLAttributes<HTMLImageElement>,
  "src" | "width" | "height"
> & {
  src: string | { src: string };
  width?: number | string;
  height?: number | string;
  fill?: boolean;
  priority?: boolean;
  quality?: number;
  placeholder?: string;
  blurDataURL?: string;
  unoptimized?: boolean;
  loader?: unknown;
};

const Image = forwardRef<HTMLImageElement, NextImageProps>(function Image(
  {
    src,
    width,
    height,
    fill,
    priority,
    quality: _quality,
    placeholder: _placeholder,
    blurDataURL: _blurDataURL,
    unoptimized: _unoptimized,
    loader: _loader,
    style,
    alt = "",
    ...props
  },
  ref,
) {
  const resolved = typeof src === "string" ? src : src.src;
  const fillStyle: CSSProperties | undefined = fill
    ? {
        position: "absolute",
        inset: 0,
        width: "100%",
        height: "100%",
        ...style,
      }
    : style;
  return (
    <img
      {...props}
      ref={ref}
      src={resolved}
      alt={alt}
      width={fill ? undefined : width}
      height={fill ? undefined : height}
      style={fillStyle}
      loading={priority ? "eager" : props.loading}
      fetchPriority={priority ? "high" : props.fetchPriority}
    />
  );
});

export default Image;
