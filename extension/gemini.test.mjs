// Run with: node --test extension/gemini.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  findLoadingBundle,
  findQueryId,
  parseUsageResponse,
  queryUrl,
} from "./gemini.js";

test("finds the loading bundle in the import map", () => {
  // Trimmed from the live page on 2026-11-07.
  const html =
    '"@messages/assets/UsagePageError-CxxDQv1x.js":"https://www.gstatic.com/bg3/boq-bard-web/islands/assets/en/UsagePageError-messages-CkuiEkmg.js","@messages/assets/UsagePageLoading-M1vUY-Sc.js":"https://www.gstatic.com/bg3/boq-bard-web/islands/assets/en/UsagePageLoading-messages-x.js"';
  assert.equal(findLoadingBundle(html), "UsagePageLoading-M1vUY-Sc.js");
  assert.equal(findLoadingBundle("<html></html>"), "");
});

test("finds the persisted query id", () => {
  // Trimmed from UsagePageLoading-M1vUY-Sc.js.
  const js =
    "var y={kind:`PreloadableConcreteRequest`,params:{id:`2/XQ1vHn3HQ1hytWNw5je1Yd8LjWBMmGmoyAexmi1hYYk=`,metadata:{},name:`UsagePageQuery`,operationKind:`query`,text:null}}";
  assert.equal(findQueryId(js), "2/XQ1vHn3HQ1hytWNw5je1Yd8LjWBMmGmoyAexmi1hYYk=");
  assert.equal(findQueryId("id:`x`,metadata:{},name:`OtherQuery`"), "");
});

test("builds the query URL", () => {
  const u = new URL(queryUrl("2/abc+def="));
  assert.equal(u.origin, "https://gemini.google.com");
  assert.equal(u.pathname, "/_/BardChatUi/graphql/schemas/GEMINI_WEB_GRAPHQL/executeQuery");
  assert.equal(u.searchParams.get("query_signature"), "2/abc+def=");
  // Both variables are non-null in UsagePageQuery; Google refuses the query
  // without workspaceRequest.
  const vars = JSON.parse(u.searchParams.get("variables"));
  assert.equal(vars.g1Request.entrypoints.length, 3);
  assert.equal(
    vars.workspaceRequest.recommendationsForSurfaceRequests[0].recommendationSurface.rpOnramp,
    "GEMINI_CHAT_USAGE_LIMITS_UPSELL",
  );
});

test("reads the pools from the answer", () => {
  const text = `)]}'
[{"data":{"viewer":{"rpcs":{"getUsageInfo":{"subscriptionTier":"TIER1","pools":[
  {"poolType":"GXU_FIVE_HOURLY","percentageUsed":0.42,"gxu":{"windowRefreshesAt":"2026-11-07T21:45:00Z"}},
  {"poolType":"GXU_WEEKLY","percentageUsed":0.1,"gxu":{"windowRefreshesAt":"2026-11-10T04:45:00Z"}},
  {"poolType":"OVERAGE_CREDITS","remainingCredits":"0"}
]}}}},"extensions":{"is_final":false}},
{"incremental":[],"extensions":{"is_final":true}}]`;
  assert.deepEqual(parseUsageResponse(text), {
    tier: "TIER1",
    pools: [
      { poolType: "GXU_FIVE_HOURLY", percentageUsed: 0.42, windowRefreshesAt: "2026-11-07T21:45:00Z" },
      { poolType: "GXU_WEEKLY", percentageUsed: 0.1, windowRefreshesAt: "2026-11-10T04:45:00Z" },
      { poolType: "OVERAGE_CREDITS", percentageUsed: null, windowRefreshesAt: null },
    ],
  });
});

test("reports a GraphQL error or a page that is not an answer", () => {
  assert.throws(
    () => parseUsageResponse(`)]}'\n[{"errors":[{"message":"PERSISTED_QUERY_NOT_FOUND"}]}]`),
    /PERSISTED_QUERY_NOT_FOUND/,
  );
  assert.throws(() => parseUsageResponse("<!doctype html><title>Sign in</title>"), /not a GraphQL/);
  assert.throws(() => parseUsageResponse(`)]}'\n[{"data":{"viewer":null}}]`), /no usage info/);
});
