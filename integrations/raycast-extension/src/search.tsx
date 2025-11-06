import { Action, ActionPanel, Icon, List, Toast, getPreferenceValues, open, showHUD, showToast } from "@raycast/api";
import { useEffect, useMemo, useRef, useState } from "react";
import { execFile } from "child_process";
import { promisify } from "util";
import fs from "fs";
import os from "os";
import path from "path";

const execFileAsync = promisify(execFile);
const DEFAULT_RESULT_LIMIT = 15;
const SEARCH_DEBOUNCE_MS = 300;
const CLI_TIMEOUT_MS = 5000;
const ADDITIONAL_PATH_DIRS = [
  path.join(os.homedir(), ".local", "bin"),
  path.join(os.homedir(), ".local", "sbin"),
  path.join(os.homedir(), ".cargo", "bin"),
  path.join(os.homedir(), "bin"),
  "/opt/homebrew/bin",
  "/opt/homebrew/sbin",
  "/usr/local/opt/arrowhead/bin",
  "/usr/local/bin",
  "/usr/local/sbin",
];
const ALTERNATE_EDITOR: Record<EditorChoice, EditorChoice> = {
  obsidian: "default",
  default: "obsidian",
};

type EditorChoice = "obsidian" | "default";

type Preferences = {
  searchMode: "fts" | "semantic" | "hybrid";
  resultLimit?: string;
  primaryEditor: EditorChoice;
  vaultPath?: string;
  arrowheadCliPath?: string;
};

type SearchResult = {
  note_id?: string;
  title?: string;
  relative_path?: string;
  absolute_path?: string;
  preview?: string;
  reason?: string;
  score?: number;
  bm25?: number;
  metadata?: Record<string, unknown>;
};

type SearchPayload = SearchResult[];

type QueryError = {
  title: string;
  message?: string;
};

type CommandState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ready"; results: SearchPayload }
  | { kind: "error"; error: QueryError };

export default function Command() {
  const preferences = getPreferenceValues<Preferences>();
  const [query, setQuery] = useState("");
  const [state, setState] = useState<CommandState>({ kind: "idle" });

  const config = useMemo(() => {
    const limit = parseInt(preferences.resultLimit ?? "", 10);
    const resultLimit = Number.isFinite(limit) && limit > 0 ? limit : DEFAULT_RESULT_LIMIT;
    return {
      searchMode: normalizeSearchMode(preferences.searchMode),
      resultLimit,
      primaryEditor: normalizeEditor(preferences.primaryEditor),
      vaultPath: normalizeVault(preferences.vaultPath),
      cliPath: resolveCliPath(preferences.arrowheadCliPath),
    };
  }, [preferences]);

  useSearchEffect(query, config, setState);

  const placeholder = useMemo(() => {
    const modeLabel = config.searchMode.toUpperCase();
    const editorLabel = describeEditor(config.primaryEditor);
    return {
      title: "Search Arrowhead notes",
      description: `${modeLabel} search • Enter a query to fetch notes (Return opens in ${editorLabel}).`,
    };
  }, [config]);

  const secondaryEditor = ALTERNATE_EDITOR[config.primaryEditor];
  const showDetail = state.kind === "ready" && state.results.some((item) => Boolean(item.preview?.trim()));

  return (
    <List
      throttle
      isLoading={state.kind === "loading"}
      isShowingDetail={showDetail}
      searchText={query}
      onSearchTextChange={setQuery}
      searchBarPlaceholder="Find notes with Arrowhead…"
    >
      {query.trim().length === 0 ? (
        <List.EmptyView icon={Icon.MagnifyingGlass} title={placeholder.title} description={placeholder.description} />
      ) : null}
      {state.kind === "error" ? (
        <List.EmptyView icon={Icon.ExclamationMark} title={state.error.title} description={state.error.message} />
      ) : null}
      {state.kind === "ready" && state.results.length === 0 ? (
        <List.EmptyView
          icon={Icon.Info}
          title="No notes matched"
          description={`Query "${query}" returned no results.`}
        />
      ) : null}
      {state.kind === "ready"
        ? state.results.map((result) => {
            const noteId = result.note_id ?? "";
            const title = selectTitle(result);
            const subtitle = selectSubtitle(result);
            const absolutePath = resolveAbsolutePath(result, config.vaultPath);
            const relativePath = result.relative_path;
            const preview = result.preview?.trim();

            return (
              <List.Item
                key={noteId || title}
                title={title || "(untitled note)"}
                subtitle={relativePath}
                keywords={collectKeywords(result)}
                accessories={buildAccessories(result)}
                icon={Icon.Document}
                detail={
                  preview ? (
                    <List.Item.Detail markdown={`\`\`\`\n${condenseWhitespace(preview)}\n\`\`\``} />
                  ) : undefined
                }
                actions={
                  <ActionPanel>
                    <Action
                      title={`Open in ${describeEditor(config.primaryEditor)}`}
                      icon={config.primaryEditor === "obsidian" ? Icon.AppWindowList : Icon.Document}
                      onAction={() => openNote(result, config.primaryEditor, config.vaultPath)}
                    />
                    <Action
                      title={`Open in ${describeEditor(secondaryEditor)}`}
                      icon={secondaryEditor === "obsidian" ? Icon.AppWindowList : Icon.Document}
                      shortcut={{ modifiers: ["cmd"], key: "return" }}
                      onAction={() => openNote(result, secondaryEditor, config.vaultPath)}
                    />
                    {absolutePath ? (
                      <>
                        <Action.ShowInFinder path={absolutePath} />
                        <Action.CopyToClipboard
                          title="Copy Note Path"
                          content={absolutePath}
                          shortcut={{ modifiers: ["cmd", "shift"], key: "c" }}
                        />
                      </>
                    ) : (
                      <Action.CopyToClipboard
                        title="Copy Note ID"
                        content={noteId}
                        shortcut={{ modifiers: ["cmd", "shift"], key: "c" }}
                      />
                    )}
                    <Action.CopyToClipboard
                      title="Copy Preview"
                      content={preview ?? ""}
                      shortcut={{ modifiers: ["ctrl"], key: "c" }}
                    />
                  </ActionPanel>
                }
              />
            );
          })
        : null}
    </List>
  );
}

function useSearchEffect(
  query: string,
  config: {
    searchMode: "fts" | "semantic" | "hybrid";
    resultLimit: number;
    cliPath: string;
    vaultPath?: string;
    primaryEditor: EditorChoice;
  },
  setState: (state: CommandState) => void,
) {
  const lastQueryRef = useRef<string>("");

  useEffect(() => {
    const trimmed = query.trim();
    if (trimmed.length === 0) {
      setState({ kind: "idle" });
      lastQueryRef.current = "";
      return;
    }

    let isCancelled = false;
    setState({ kind: "loading" });

    const timer = setTimeout(async () => {
      try {
        const { results } = await runSearch(trimmed, config);
        if (isCancelled) {
          return;
        }
        lastQueryRef.current = trimmed;
        setState({ kind: "ready", results });
      } catch (error) {
        if (isCancelled) {
          return;
        }
        const message = error instanceof Error ? error.message : String(error);
        setState({ kind: "error", error: { title: "arrowhead search failed", message } });
        await showToast({
          style: Toast.Style.Failure,
          title: "Arrowhead search failed",
          message,
        });
      }
    }, SEARCH_DEBOUNCE_MS);

    return () => {
      isCancelled = true;
      clearTimeout(timer);
    };
  }, [query, config.cliPath, config.resultLimit, config.searchMode, config.vaultPath, setState]);
}

async function runSearch(
  query: string,
  config: {
    searchMode: "fts" | "semantic" | "hybrid";
    resultLimit: number;
    cliPath: string;
    vaultPath?: string;
  },
): Promise<{ results: SearchPayload }> {
  const env = augmentPath(process.env);
  const args = [
    "search",
    config.searchMode,
    query,
    "--json",
    "--limit",
    String(config.resultLimit),
    "--include-paths",
  ];

  if (config.vaultPath && !args.includes("--vault")) {
    args.push("--vault", config.vaultPath);
  }

  try {
    const { stdout } = await execFileAsync(config.cliPath, args, {
      env,
      timeout: CLI_TIMEOUT_MS,
      maxBuffer: 10 * 1024 * 1024,
    });
    const payload = JSON.parse(stdout);
    if (!Array.isArray(payload)) {
      throw new Error("Unexpected search payload (expected JSON array).");
    }
    return { results: payload as SearchPayload };
  } catch (error) {
    if (isNotFoundError(error)) {
      throw new Error("arrowhead CLI not found. Adjust the extension preferences.");
    }
    if (error instanceof Error) {
      const execError = error as NodeJS.ErrnoException & { killed?: boolean; stderr?: string };
      if (execError.killed) {
        throw new Error("arrowhead search timed out. Try lowering the result limit.");
      }
      if (typeof execError.stderr === "string") {
        const stderr = execError.stderr.trim();
        if (stderr.length > 0) {
          throw new Error(stderr.split("\n")[0]);
        }
      }
      throw error;
    }
    throw new Error(String(error));
  }
}

async function openNote(result: SearchResult, editor: EditorChoice, vaultPath?: string) {
  const absolutePath = resolveAbsolutePath(result, vaultPath);
  const noteId = result.note_id ?? "";

  if (!absolutePath) {
    await showToast({
      style: Toast.Style.Failure,
      title: "Unable to resolve note path",
      message: "Check your vault configuration.",
    });
    return;
  }

  if (editor === "obsidian") {
    const uri = obsidianUri(absolutePath, vaultPath);
    if (uri) {
      await open(uri);
      return;
    }
    await openWithApp(absolutePath, "md.obsidian");
    return;
  }

  await open(absolutePath);
  await showHUD(`Opened ${noteId || path.basename(absolutePath)}`);
}

function resolveCliPath(override?: string): string {
  if (override && override.trim().length > 0) {
    const expanded = expandHome(override.trim());
    return expanded;
  }

  const envOverride = process.env.ARROWHEAD_CLI_PATH;
  if (envOverride && envOverride.trim().length > 0) {
    return expandHome(envOverride.trim());
  }

  const candidates = collectPathCandidates();
  for (const candidate of candidates) {
    if (isExecutable(candidate)) {
      return candidate;
    }
  }
  return "arrowhead";
}

function collectPathCandidates(): string[] {
  const currentPaths = (process.env.PATH ?? "")
    .split(path.delimiter)
    .filter(Boolean)
    .map((entry) => path.join(entry, "arrowhead"));
  const fallback = ADDITIONAL_PATH_DIRS.map((dir) => path.join(dir, "arrowhead"));
  return [...currentPaths, ...fallback];
}

function isExecutable(candidate: string): boolean {
  try {
    const stats = fs.statSync(candidate);
    if (!stats.isFile()) {
      return false;
    }
    fs.accessSync(candidate, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function augmentPath(env: NodeJS.ProcessEnv): NodeJS.ProcessEnv {
  const updated = { ...env };
  const entries = new Set<string>();
  const existing = env.PATH ? env.PATH.split(path.delimiter) : [];
  for (const entry of ADDITIONAL_PATH_DIRS) {
    entries.add(entry);
  }
  for (const entry of existing) {
    entries.add(entry);
  }
  updated.PATH = Array.from(entries).join(path.delimiter);
  return updated;
}

function resolveAbsolutePath(result: SearchResult, vaultPath?: string): string | undefined {
  const absolute = result.absolute_path?.trim();
  if (absolute) {
    return absolute;
  }
  if (vaultPath) {
    const relative = result.relative_path?.trim();
    if (relative) {
      const joined = path.join(vaultPath, relative);
      if (fs.existsSync(joined)) {
        return path.resolve(joined);
      }
    }
    const noteId = result.note_id?.trim();
    if (noteId) {
      const candidate = path.join(vaultPath, `${noteId}.md`);
      if (fs.existsSync(candidate)) {
        return path.resolve(candidate);
      }
    }
  }
  return undefined;
}

function obsidianUri(notePath: string, vaultPath?: string): string | undefined {
  try {
    const resolved = path.resolve(notePath);
    const encodedPath = encodeURIComponent(resolved);
    const params = [`path=${encodedPath}`];
    if (vaultPath) {
      const vaultName = path.basename(vaultPath);
      if (vaultName.trim().length > 0) {
        params.push(`vault=${encodeURIComponent(vaultName)}`);
      }
    }
    return `obsidian://open?${params.join("&")}`;
  } catch {
    return undefined;
  }
}

async function openWithApp(filePath: string, bundleId: string) {
  const args = ["-b", bundleId, filePath];
  try {
    await execFileAsync("open", args, { timeout: CLI_TIMEOUT_MS });
  } catch (error) {
    await open(filePath);
    if (error instanceof Error) {
      await showToast({
        style: Toast.Style.Animated,
        title: "Opened with default editor",
        message: error.message,
      });
    }
  }
}

function normalizeSearchMode(value: string | undefined): "fts" | "semantic" | "hybrid" {
  if (!value) {
    return "hybrid";
  }
  const normalised = value.toLowerCase();
  if (normalised === "fts" || normalised === "semantic" || normalised === "hybrid") {
    return normalised;
  }
  return "hybrid";
}

function normalizeEditor(value: string | undefined): EditorChoice {
  if (!value) {
    return "obsidian";
  }
  const normalised = value.toLowerCase();
  return normalised === "default" ? "default" : "obsidian";
}

function normalizeVault(value: string | undefined): string | undefined {
  if (!value || value.trim().length === 0) {
    return inferVaultPath();
  }
  const expanded = expandHome(value.trim());
  return fs.existsSync(expanded) ? expanded : undefined;
}

function inferVaultPath(): string | undefined {
  const envOverride = process.env.VAULT_PATH ?? process.env.ARROWHEAD_VAULT_PATH;
  if (envOverride && envOverride.trim().length > 0) {
    const expanded = expandHome(envOverride.trim());
    if (fs.existsSync(expanded)) {
      return expanded;
    }
  }

  const configOverride = process.env.ARROWHEAD_CONFIG_PATH;
  for (const candidate of configCandidates(configOverride)) {
    const pathFromConfig = parseVaultFromConfig(candidate);
    if (pathFromConfig) {
      return pathFromConfig;
    }
  }
  return undefined;
}

function configCandidates(override?: string): string[] {
  if (override && override.trim().length > 0) {
    return [expandHome(override.trim())];
  }

  const home = os.homedir();
  const candidates = [];
  if (process.platform === "darwin") {
    candidates.push(path.join(home, "Library", "Application Support", "Arrowhead", "config.toml"));
  } else if (process.platform === "win32") {
    const appData = process.env.APPDATA || path.join(home, "AppData", "Roaming");
    candidates.push(path.join(appData, "Arrowhead", "config.toml"));
  } else {
    const xdgConfig = process.env.XDG_CONFIG_HOME || path.join(home, ".config");
    candidates.push(path.join(xdgConfig, "Arrowhead", "config.toml"));
  }
  candidates.push(path.join(home, ".config", "arrowhead", "config.toml"));
  return candidates;
}

function parseVaultFromConfig(configPath: string): string | undefined {
  try {
    const content = fs.readFileSync(configPath, "utf8");
    const lines = content.split(/\r?\n/);
    for (const rawLine of lines) {
      const line = rawLine.trim();
      if (!line || line.startsWith("#")) {
        continue;
      }
      if (line.toLowerCase().startsWith("vault")) {
        const [, rhs] = line.split("=", 2);
        if (!rhs) {
          continue;
        }
        const withoutComment = rhs.split("#", 1)[0].trim();
        const parsed = parseTomlString(withoutComment);
        if (parsed) {
          const expanded = expandHome(parsed);
          if (fs.existsSync(expanded)) {
            return expanded;
          }
        }
        break;
      }
    }
  } catch {
    // Ignore read/parsing failures.
  }
  return undefined;
}

function parseTomlString(value: string): string | undefined {
  if (value.startsWith('"') && value.endsWith('"')) {
    try {
      return JSON.parse(value);
    } catch {
      return undefined;
    }
  }
  if (value.startsWith("'") && value.endsWith("'")) {
    return value.slice(1, -1);
  }
  return undefined;
}

function expandHome(input: string): string {
  if (!input.startsWith("~")) {
    return input;
  }
  return path.join(os.homedir(), input.slice(1));
}

function selectTitle(result: SearchResult): string {
  if (result.title && result.title.trim().length > 0) {
    return result.title.trim();
  }
  const metadata = result.metadata;
  if (metadata) {
    const metaTitle = metadata["title"];
    if (typeof metaTitle === "string" && metaTitle.trim().length > 0) {
      return metaTitle.trim();
    }
  }
  return result.note_id ?? "";
}

function selectSubtitle(result: SearchResult): string {
  const segments: string[] = [];
  if (result.preview) {
    segments.push(condenseWhitespace(result.preview));
  }
  const scores: string[] = [];
  if (typeof result.score === "number") {
    scores.push(`score ${result.score.toFixed(3)}`);
  }
  if (typeof result.bm25 === "number") {
    scores.push(`BM25 ${result.bm25.toFixed(2)}`);
  }
  if (scores.length > 0) {
    segments.push(scores.join(" • "));
  }
  if (result.reason && segments.length === 0) {
    segments.push(result.reason);
  }
  return segments.join(" — ");
}

function collectKeywords(result: SearchResult): string[] {
  const keywords = new Set<string>();
  if (result.title) {
    keywords.add(result.title);
  }
  if (result.note_id) {
    keywords.add(result.note_id);
  }
  if (result.relative_path) {
    keywords.add(result.relative_path);
  }
  if (result.reason) {
    keywords.add(result.reason);
  }
  if (result.preview) {
    keywords.add(condenseWhitespace(result.preview));
  }
  return Array.from(keywords).filter(Boolean);
}

function buildAccessories(result: SearchResult): List.Item.Accessory[] {
  const accessories: List.Item.Accessory[] = [];
  if (typeof result.score === "number") {
    accessories.push({ text: `score ${result.score.toFixed(3)}` });
  }
  if (typeof result.bm25 === "number") {
    accessories.push({ text: `BM25 ${result.bm25.toFixed(2)}` });
  }
  if (result.reason) {
    accessories.push({ tooltip: result.reason, icon: Icon.Info });
  }
  return accessories;
}

function describeEditor(editor: EditorChoice): string {
  return editor === "obsidian" ? "Obsidian" : "the default editor";
}

function condenseWhitespace(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

function isNotFoundError(error: unknown): boolean {
  return Boolean(error && typeof error === "object" && "code" in error && (error as NodeJS.ErrnoException).code === "ENOENT");
}
