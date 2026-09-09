import { LoroDoc, LoroMap } from "loro-crdt";
import { describe, expect, it } from "vitest";
import { joinContinuations, type SessionMessageEntry } from "./messages";
import { materializeTail } from "./tail";

const entry = (id: string, continuationOf?: string): SessionMessageEntry => ({
  id,
  role: "assistant",
  parts: [{ kind: "text", id, text: id }],
  createdAt: 1,
  deviceId: "host",
  ...(continuationOf ? { continuationOf } : {})
});

describe("session tail projection", () => {
  it("joins long continuation chains without changing input roots or hiding orphans", () => {
    const root = entry("root");
    const orphan = entry("orphan", "missing");
    const continuations = Array.from({ length: 1000 }, (_, index) => entry(`part-${index}`, root.id));
    const input = [root, orphan, ...continuations, entry("last")];
    const joined = joinContinuations(input);

    expect(joined.map((message) => message.id)).toEqual(["root", "orphan", "last"]);
    expect(joined[0]?.parts.map((part) => part.id)).toEqual([root, ...continuations].map((message) => message.id));
    expect(root.parts).toEqual([{ kind: "text", id: "root", text: "root" }]);
    expect(joinContinuations(input)).toEqual(joined);
  });

  it("returns the last logical messages with continuations and identifying metadata", () => {
    const doc = new LoroDoc();
    const messages = doc.getList("messages");
    const meta = doc.getMap("meta");
    const unrelated = doc.getText("unrelated");
    try {
      meta.set("chatId", "tail-chat");
      meta.set("schemaVersion", 7);
      unrelated.insert(0, "not transcript".repeat(1000));
      for (const message of [entry("old"), entry("root"), entry("part", "root"), entry("orphan", "missing")]) {
        const detached = new LoroMap();
        const map = messages.insertContainer(messages.length, detached);
        try {
          for (const [key, value] of Object.entries(message)) map.set(key, value);
        } finally {
          map.free();
          detached.free();
        }
      }
      expect(materializeTail(doc, 123, 2)).toEqual({
        chatId: "tail-chat",
        schemaVersion: 7,
        messages: [
          { ...entry("root"), parts: [...entry("root").parts, ...entry("part", "root").parts] },
          entry("orphan", "missing")
        ],
        totalMessages: 3,
        updatedAt: 123
      });
    } finally {
      unrelated.free();
      meta.free();
      messages.free();
      doc.free();
    }
  });
});
