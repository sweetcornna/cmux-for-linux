import { describe, expect, test } from "bun:test";
import { createCodeMountNotifier } from "./codeSurface";

describe("code surface mount", () => {
  test("starts the native sidecar once when the launcher mounts", () => {
    const messages: unknown[] = [];
    const notify = createCodeMountNotifier((message) => messages.push(message));
    const element = {} as HTMLElement;

    notify(null);
    notify(element);
    notify(element);
    notify(null);
    notify(element);

    expect(messages).toEqual([{ type: "mount" }]);
  });
});
