import assert from "node:assert/strict";
import test from "node:test";

import { isExplicitTemplateRuntimePin } from "./useApplyTemplate.ts";

test("an explicit available template runtime is pinned", () => {
  assert.equal(isExplicitTemplateRuntimePin("goose", "goose"), true);
});

test("an unavailable explicit runtime falling back to OMPK is not pinned", () => {
  assert.equal(isExplicitTemplateRuntimePin("goose", "ompk"), false);
});

test("an implicit template runtime is not pinned", () => {
  assert.equal(isExplicitTemplateRuntimePin(null, "ompk"), false);
});
