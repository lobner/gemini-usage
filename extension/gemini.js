// The pure parts of the bridge: finding the usage query in the Gemini web
// app's bundles, and reading its answer. No browser APIs here, so Node can
// test it (gemini.test.mjs).
//
// The usage page at gemini.google.com/usage runs a persisted Relay query,
// UsagePageQuery. Its id changes with every deploy of the web app, so the
// bridge finds the current one:
//
// 1. The page HTML has an import map. It names the current
//    UsagePageLoading-<hash>.js bundle.
// 2. That bundle holds the query's params: {id:`2/...`, name:`UsagePageQuery`}.
//
// The query answers with viewer.rpcs.getUsageInfo.pools: one pool per window,
// with poolType (GXU_FIVE_HOURLY, GXU_WEEKLY, OVERAGE_CREDITS), percentageUsed
// (a fraction, 0-1) and gxu.windowRefreshesAt (RFC 3339).

export const ORIGIN = "https://gemini.google.com";
export const ASSET_BASE = "https://www.gstatic.com/bg3/boq-bard-web/islands/assets/";
export const QUERY_PATH = "/_/BardChatUi/graphql/schemas/GEMINI_WEB_GRAPHQL/executeQuery";

// The variables that the usage page sends with the query (from the "usage"
// route's preload in usage-<hash>.js). Both are non-null in the query.
export const QUERY_VARIABLES = {
  g1Request: {
    entrypoints: [
      "GEMINI_USAGE_SETTINGS_UPGRADE_BANNER",
      "GEMINI_USAGE_SETTINGS_AI_CREDITS_UPSELL_CARD",
      "GEMINI_USAGE_SETTINGS_UPGRADE_AND_CREDITS_BANNER",
    ],
  },
  workspaceRequest: {
    recommendationsForSurfaceRequests: [
      {
        recommendationSurface: { rpOnramp: "GEMINI_CHAT_USAGE_LIMITS_UPSELL", version: "1" },
        maxRecommendationCount: 1,
      },
    ],
  },
};

/** Returns the file name of the UsagePageLoading bundle in the page HTML, or "". */
export function findLoadingBundle(html) {
  const m = /@messages\/assets\/(UsagePageLoading-[\w-]+\.js)/.exec(html);
  return m ? m[1] : "";
}

/** Returns the persisted id of UsagePageQuery in a bundle, or "". */
export function findQueryId(js) {
  const m = /id:`([^`]+)`,metadata:\{\},name:`UsagePageQuery`/.exec(js);
  return m ? m[1] : "";
}

/** Builds the GET URL of the query, as the web app does for a query. */
export function queryUrl(queryId) {
  const params = new URLSearchParams({
    query_signature: queryId,
    variables: JSON.stringify(QUERY_VARIABLES),
  });
  return `${ORIGIN}${QUERY_PATH}?${params}`;
}

/**
 * Reads the query's answer: a ")]}'" guard, then a JSON array of payloads
 * (the first one has the data; later ones carry @defer parts). Returns
 * {tier, pools} with only the fields the app needs, or throws.
 */
export function parseUsageResponse(text) {
  const start = text.indexOf("[");
  if (start < 0) {
    throw new Error("the answer is not a GraphQL payload list");
  }
  let payloads;
  try {
    payloads = JSON.parse(text.slice(start));
  } catch (e) {
    throw new Error(`the answer is not JSON: ${e.message}`);
  }
  if (!Array.isArray(payloads)) {
    payloads = [payloads];
  }
  for (const p of payloads) {
    const info = p?.data?.viewer?.rpcs?.getUsageInfo;
    if (info && Array.isArray(info.pools)) {
      return {
        tier: typeof info.subscriptionTier === "string" ? info.subscriptionTier : "",
        pools: info.pools
          .filter((pool) => typeof pool?.poolType === "string")
          .map((pool) => ({
            poolType: pool.poolType,
            percentageUsed: typeof pool.percentageUsed === "number" ? pool.percentageUsed : null,
            windowRefreshesAt: pool.gxu?.windowRefreshesAt ?? null,
          })),
      };
    }
  }
  const err = payloads.find((p) => Array.isArray(p?.errors))?.errors?.[0]?.message;
  throw new Error(err ? `Gemini said: ${err}` : "the answer has no usage info");
}
