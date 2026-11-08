import assert from "node:assert/strict";
import test from "node:test";
import { capabilitySettingsErrors } from "../lib/capability-settings-validation";

test("非法轮次给出完整字段、取值和范围且不修改草稿", () => {
  const draft = { chat: { max_rounds: 330, temperature: 0.2, stage_budgets: { exploring: 1600, responding: 8000 } } };
  const errors = capabilitySettingsErrors(draft);
  assert.equal(errors.length, 1);
  assert.match(errors[0], /capabilities\.chat\.max_rounds/);
  assert.match(errors[0], /1–50/);
  assert.match(errors[0], /330/);
  assert.equal(draft.chat.max_rounds, 330);
  draft.chat.max_rounds = 32;
  assert.deepEqual(capabilitySettingsErrors(draft), []);
});

test("整数小数、空值、嵌套越界均不能静默写入", () => {
  for (const value of [null, 2.2, 0, 51, Infinity]) {
    assert.equal(capabilitySettingsErrors({ chat: { max_rounds: value } }).length, 1);
  }
  assert.equal(capabilitySettingsErrors({ research: { researching: { tool_timeout: 601 } } }).length, 1);
});
