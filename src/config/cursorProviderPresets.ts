import type { ProviderCategory } from "../types";
import type { TemplateValueConfig } from "./claudeProviderPresets";

export interface CursorProviderPreset {
  name: string;
  websiteUrl: string;
  settingsConfig: {
    env: {
      OPENAI_API_KEY?: string;
      OPENAI_BASE_URL: string;
      OPENAI_MODEL?: string;
    };
  };
  category?: ProviderCategory;
  isPartner?: boolean;
  partnerPromotionKey?: string;
  icon?: string;
  iconColor?: string;
  templateValues?: Record<string, TemplateValueConfig>;
}

export const cursorProviderPresets: CursorProviderPreset[] = [
  {
    name: "DeepSeek",
    websiteUrl: "https://platform.deepseek.com",
    settingsConfig: {
      env: {
        OPENAI_BASE_URL: "https://api.deepseek.com/v1",
        OPENAI_MODEL: "deepseek-chat",
      },
    },
    category: "official",
    icon: "deepseek",
  },
  {
    name: "OpenAI",
    websiteUrl: "https://platform.openai.com",
    settingsConfig: {
      env: {
        OPENAI_BASE_URL: "https://api.openai.com/v1",
        OPENAI_MODEL: "gpt-4o",
      },
    },
    category: "official",
    icon: "openai",
  },
];
