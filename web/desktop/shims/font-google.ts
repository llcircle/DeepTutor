type FontOptions = {
  subsets?: string[];
  display?: string;
  variable?: string;
  weight?: string | string[];
  style?: string;
};

function localFont(options: FontOptions = {}) {
  return {
    className: "",
    variable: options.variable ?? "",
    style: {
      fontFamily: "inherit",
      fontStyle: options.style ?? "normal",
    },
  };
}

export const Geist = localFont;
export const Geist_Mono = localFont;
export const Lora = localFont;
export const Inter = localFont;
export const Roboto = localFont;
