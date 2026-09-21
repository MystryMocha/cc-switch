import { useMemo, useState } from "react";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import {
  Form,
  FormControl,
  FormField,
  FormItem,
  FormLabel,
  FormMessage,
} from "@/components/ui/form";
import { Input } from "@/components/ui/input";
import { providerSchema, type ProviderFormData } from "@/lib/schemas/provider";
import type { ProviderCategory, ProviderMeta } from "@/types";
import type { ProviderFormProps, ProviderFormValues } from "./ProviderForm";
import { BasicFormFields } from "./BasicFormFields";
import { ApiKeySection, EndpointField } from "./shared";
import {
  cursorProviderPresets,
  type CursorProviderPreset,
} from "@/config/cursorProviderPresets";

type CursorProviderFormProps = Omit<ProviderFormProps, "appId">;

function envOf(config?: Record<string, unknown>) {
  const env = config?.env;
  if (!env || typeof env !== "object") return {};
  return env as Record<string, string>;
}

export function CursorProviderForm({
  submitLabel,
  onSubmit,
  onCancel,
  onSubmittingChange,
  initialData,
  showButtons = true,
}: CursorProviderFormProps) {
  const { t } = useTranslation();
  const initialEnv = envOf(initialData?.settingsConfig);
  const [apiKey, setApiKey] = useState(initialEnv.OPENAI_API_KEY ?? "");
  const [baseUrl, setBaseUrl] = useState(initialEnv.OPENAI_BASE_URL ?? "");
  const [model, setModel] = useState(initialEnv.OPENAI_MODEL ?? "");
  const [selectedPresetId, setSelectedPresetId] = useState<string | null>(
    initialData ? null : "custom",
  );
  const [category, setCategory] = useState<ProviderCategory | undefined>(
    initialData?.category ?? "custom",
  );

  const presetEntries = useMemo(
    () =>
      cursorProviderPresets.map((preset, index) => ({
        id: `cursor-${index}`,
        preset,
      })),
    [],
  );

  const form = useForm<ProviderFormData>({
    resolver: zodResolver(providerSchema),
    defaultValues: {
      name: initialData?.name ?? "",
      websiteUrl: initialData?.websiteUrl ?? "",
      notes: initialData?.notes ?? "",
      settingsConfig: "{}",
      icon: initialData?.icon ?? "cursor",
      iconColor: initialData?.iconColor ?? "",
    },
  });

  const handlePresetChange = (id: string | null) => {
    setSelectedPresetId(id);
    if (!id || id === "custom") {
      setCategory("custom");
      return;
    }
    const entry = presetEntries.find((item) => item.id === id);
    if (!entry) return;
    const preset = entry.preset as CursorProviderPreset;
    form.setValue("name", preset.name);
    form.setValue("websiteUrl", preset.websiteUrl);
    form.setValue("icon", preset.icon ?? "cursor");
    setBaseUrl(preset.settingsConfig.env.OPENAI_BASE_URL);
    setModel(preset.settingsConfig.env.OPENAI_MODEL ?? "");
    setCategory(preset.category ?? "custom");
  };

  const handleSubmit = async (values: ProviderFormData) => {
    const name = values.name.trim();
    if (!name) {
      toast.error(t("provider.enterName"));
      return;
    }
    if (!baseUrl.trim() || !apiKey.trim()) {
      toast.error(
        t("cursor.form.required", {
          defaultValue: "请填写 API Key 和 Override OpenAI Base URL",
        }),
      );
      return;
    }
    onSubmittingChange?.(true);
    try {
      const meta: ProviderMeta = {
        ...(initialData?.meta ?? {}),
      };
      const payload: ProviderFormValues = {
        ...values,
        name,
        websiteUrl: values.websiteUrl?.trim() ?? "",
        settingsConfig: JSON.stringify({
          env: {
            OPENAI_API_KEY: apiKey.trim(),
            OPENAI_BASE_URL: baseUrl.trim().replace(/\/+$/, ""),
            ...(model.trim() ? { OPENAI_MODEL: model.trim() } : {}),
          },
        }),
        presetId: selectedPresetId ?? undefined,
        presetCategory: category ?? "custom",
        meta,
      };
      await onSubmit(payload);
    } finally {
      onSubmittingChange?.(false);
    }
  };

  return (
    <Form {...form}>
      <form
        id="provider-form"
        onSubmit={form.handleSubmit(handleSubmit)}
        className="space-y-6 glass rounded-xl p-6 border border-white/10"
      >
        {!initialData && (
          <div className="flex flex-wrap gap-2">
            <Button
              type="button"
              variant={selectedPresetId === "custom" ? "default" : "outline"}
              size="sm"
              onClick={() => handlePresetChange("custom")}
            >
              {t("provider.custom", { defaultValue: "自定义" })}
            </Button>
            {presetEntries.map((entry) => (
              <Button
                key={entry.id}
                type="button"
                variant={selectedPresetId === entry.id ? "default" : "outline"}
                size="sm"
                onClick={() => handlePresetChange(entry.id)}
              >
                {entry.preset.name}
              </Button>
            ))}
          </div>
        )}
        <p className="text-xs text-muted-foreground">
          {t("cursor.form.hint", {
            defaultValue:
              "切换后会写入 ~/.cursor/cc-switch-provider.json，并尝试更新 Cursor IDE 的 BYOK。请重启 Cursor；若未生效，到 Settings → Models 粘贴 OpenAI API Key 和 Override Base URL。",
          })}
        </p>
        <BasicFormFields form={form} />
        <ApiKeySection
          value={apiKey}
          onChange={setApiKey}
          category={category}
          shouldShowLink={Boolean(form.watch("websiteUrl"))}
          websiteUrl={form.watch("websiteUrl") ?? ""}
        />
        <EndpointField
          id="cursor-base-url"
          label={t("cursor.form.baseUrl", {
            defaultValue: "Override OpenAI Base URL",
          })}
          value={baseUrl}
          onChange={setBaseUrl}
          placeholder="https://api.example.com/v1"
          showManageButton={false}
        />
        <FormField
          control={form.control}
          name="notes"
          render={() => (
            <FormItem>
              <FormLabel>
                {t("cursor.form.model", { defaultValue: "模型 ID" })}
              </FormLabel>
              <FormControl>
                <Input
                  value={model}
                  onChange={(event) => setModel(event.target.value)}
                  placeholder="deepseek-chat"
                />
              </FormControl>
              <FormMessage />
            </FormItem>
          )}
        />
        {showButtons && (
          <div className="flex justify-end gap-2">
            <Button type="button" variant="outline" onClick={onCancel}>
              {t("common.cancel")}
            </Button>
            <Button type="submit">{submitLabel}</Button>
          </div>
        )}
      </form>
    </Form>
  );
}
