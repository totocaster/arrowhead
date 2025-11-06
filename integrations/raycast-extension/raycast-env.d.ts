/// <reference types="@raycast/api">

/* 🚧 🚧 🚧
 * This file is auto-generated from the extension's manifest.
 * Do not modify manually. Instead, update the `package.json` file.
 * 🚧 🚧 🚧 */

/* eslint-disable @typescript-eslint/ban-types */

type ExtensionPreferences = {
  /** Search Mode - Choose which Arrowhead search strategy to run. */
  "searchMode": "hybrid" | "fts" | "semantic",
  /** Result Limit - Maximum number of results to request from Arrowhead. */
  "resultLimit": string,
  /** Primary Editor - Editor opened when hitting Return. */
  "primaryEditor": "obsidian" | "default",
  /** Vault Path Override - Optional path to an Arrowhead-compatible vault. Leave blank to read Arrowhead config. */
  "vaultPath"?: string,
  /** Arrowhead CLI Path - Override path for the Arrowhead CLI if it is not on PATH. */
  "arrowheadCliPath"?: string
}

/** Preferences accessible in all the extension's commands */
declare type Preferences = ExtensionPreferences

declare namespace Preferences {
  /** Preferences accessible in the `search` command */
  export type Search = ExtensionPreferences & {}
}

declare namespace Arguments {
  /** Arguments passed to the `search` command */
  export type Search = {}
}

