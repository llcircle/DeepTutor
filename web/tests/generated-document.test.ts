import test from "node:test";
import assert from "node:assert/strict";
import { generatedDocumentId } from "../lib/generated-document";

test("只在真实生成文档的下载入口打开作者编辑器", () => {
  const id = "a".repeat(32);
  const attachment = { type: "document", generated: true, url: `/api/documents/${id}/markdown` };
  assert.equal(generatedDocumentId(attachment), id);
  assert.equal(generatedDocumentId({ ...attachment, generated: false }), null);
  assert.equal(generatedDocumentId({ ...attachment, url: `https://example.com${attachment.url}` }), null);
  assert.equal(generatedDocumentId({ ...attachment, url: "/api/documents/../../settings/markdown" }), null);
  assert.equal(generatedDocumentId({ ...attachment, url: "/files/outputs/report.md" }), null);
});
