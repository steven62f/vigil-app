import { createContext, useContext } from "react";
import { DEFAULT_SETTINGS, type VigilSettings } from "../lib/settingsStore";

export const SettingsContext = createContext<VigilSettings>(DEFAULT_SETTINGS);

export function useSettings(): VigilSettings {
  return useContext(SettingsContext);
}
