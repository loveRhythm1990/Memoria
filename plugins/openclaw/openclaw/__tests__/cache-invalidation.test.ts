/**
 * Cache invalidation: getMemory must not serve records that a later operation
 * deleted, superseded, or made invisible.
 *
 * The cache key is `${userId}::${memoryId}` — it carries no branch or version —
 * so operations that change which memories are visible have to drop the entries
 * they can no longer vouch for, while leaving other users' entries alone.
 */
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { MemoriaClient } from "../client.js";
import { buildApiConfig, mockFetch } from "./helpers.js";

const MEMORY = {
  memory_id: "m1",
  content: "old content",
  memory_type: "semantic",
  trust_tier: "T3",
  is_active: true,
};

let originalFetch: typeof globalThis.fetch;

beforeEach(() => {
  originalFetch = globalThis.fetch;
});

afterEach(() => {
  globalThis.fetch = originalFetch;
});

/** Cache m1 for `userId` by retrieving it, then hand back the mock. */
async function cacheM1(client: MemoriaClient, f: ReturnType<typeof mockFetch>, userId = "u") {
  f.respondWith(200, [MEMORY]);
  await client.retrieve({ userId, query: "old", topK: 5 });
  // Confirm it really is cached: no further fetch should be needed.
  const before = f.calls.length;
  expect(await client.getMemory({ userId, memoryId: "m1" })).not.toBeNull();
  expect(f.calls.length).toBe(before);
}

/** After invalidation getMemory must consult the backend, which reports nothing. */
async function expectRevalidatedToNull(
  client: MemoriaClient,
  f: ReturnType<typeof mockFetch>,
  userId = "u",
) {
  f.respondWith(200, { items: [], next_cursor: null });
  const before = f.calls.length;
  expect(await client.getMemory({ userId, memoryId: "m1" })).toBeNull();
  expect(f.calls.length).toBeGreaterThan(before);
}

describe("memory cache invalidation", () => {
  it("topic purge invalidates cached memories", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(200, { purged: 1 });
      await c.purgeMemory({ userId: "u", topic: "old" });
      await expectRevalidatedToNull(c, f);
    } finally {
      c.close();
    }
  });

  it("correctById stops serving the superseded record", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(200, { memory_id: "m2", content: "new content" });
      await c.correctById({ userId: "u", memoryId: "m1", newContent: "new content" });
      await expectRevalidatedToNull(c, f);
    } finally {
      c.close();
    }
  });

  it("correctByQuery invalidates the user's cache", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(200, { memory_id: "m2", content: "new content" });
      await c.correctByQuery({ userId: "u", query: "old", newContent: "new content" });
      await expectRevalidatedToNull(c, f);
    } finally {
      c.close();
    }
  });

  it("branch checkout invalidates cached memories", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(200, { result: "Switched to branch experiment" });
      await c.branchCheckout({ userId: "u", name: "experiment" });
      // Same id, different content on the other branch.
      f.respondWith(200, { ...MEMORY, content: "experiment content" });
      const fetched = await c.getMemory({ userId: "u", memoryId: "m1" });
      expect(fetched?.content).toBe("experiment content");
    } finally {
      c.close();
    }
  });

  it("branch merge invalidates cached memories", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(200, { result: "Merged" });
      await c.branchMerge({ userId: "u", source: "experiment", strategy: "accept" });
      await expectRevalidatedToNull(c, f);
    } finally {
      c.close();
    }
  });

  it("snapshot rollback invalidates cached memories", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(200, { result: "Rolled back" });
      await c.rollbackSnapshot({ userId: "u", name: "before" });
      f.respondWith(200, { ...MEMORY, content: "restored content" });
      const fetched = await c.getMemory({ userId: "u", memoryId: "m1" });
      expect(fetched?.content).toBe("restored content");
    } finally {
      c.close();
    }
  });

  // Control from the issue: this path already worked and must keep working.
  it("deleteMemory still drops its own entry", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(200, { purged: 1 });
      await c.deleteMemory({ userId: "u", memoryId: "m1" });
      await expectRevalidatedToNull(c, f);
    } finally {
      c.close();
    }
  });

  it("purge by memory_id leaves other cached ids alone", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      f.respondWith(200, [MEMORY, { ...MEMORY, memory_id: "m9", content: "keep me" }]);
      await c.retrieve({ userId: "u", query: "old", topK: 5 });
      f.respondWith(200, { purged: 1 });
      await c.purgeMemory({ userId: "u", memoryId: "m1" });

      const before = f.calls.length;
      const kept = await c.getMemory({ userId: "u", memoryId: "m9" });
      expect(kept?.content).toBe("keep me");
      expect(f.calls.length).toBe(before);
    } finally {
      c.close();
    }
  });

  it("invalidation does not leak across users", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f, "alice");
      await cacheM1(c, f, "bob");

      f.respondWith(200, { purged: 1 });
      await c.purgeMemory({ userId: "alice", topic: "old" });

      // bob's entry is untouched: still served from cache.
      const before = f.calls.length;
      expect(await c.getMemory({ userId: "bob", memoryId: "m1" })).not.toBeNull();
      expect(f.calls.length).toBe(before);

      // alice's is gone.
      await expectRevalidatedToNull(c, f, "alice");
    } finally {
      c.close();
    }
  });

  it("a failed purge keeps the cache", async () => {
    const f = mockFetch();
    const c = new MemoriaClient(buildApiConfig());
    try {
      await cacheM1(c, f);
      f.respondWith(500, { error: "boom" });
      await expect(c.purgeMemory({ userId: "u", topic: "old" })).rejects.toThrow();

      const before = f.calls.length;
      expect(await c.getMemory({ userId: "u", memoryId: "m1" })).not.toBeNull();
      expect(f.calls.length).toBe(before);
    } finally {
      c.close();
    }
  });
});
