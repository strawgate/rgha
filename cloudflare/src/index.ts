// One Durable Object per runner name; it owns that runner's container.
//
//   POST   /runners/:name?instance=basic  {"jit": "..."}   start
//
// The instance type is fixed per container class, so there is one class
// (and binding) per size.
//   GET    /runners/:name?instance=basic                 status
//   DELETE /runners/:name?instance=basic                 stop
//
// The JIT config travels only in the request body and the container env.
import { DurableObject } from "cloudflare:workers";

interface Env {
  RUNNER_BASIC: DurableObjectNamespace<Runner>;
  RUNNER_STANDARD_1: DurableObjectNamespace<Runner>;
  RUNNER_STANDARD_2: DurableObjectNamespace<Runner>;
  RUNNER_STANDARD_4: DurableObjectNamespace<Runner>;
  RGHA_TOKEN: string;
}

const SIZES = {
  basic: (e: Env) => e.RUNNER_BASIC,
  "standard-1": (e: Env) => e.RUNNER_STANDARD_1,
  "standard-2": (e: Env) => e.RUNNER_STANDARD_2,
  "standard-4": (e: Env) => e.RUNNER_STANDARD_4,
} as const;
type Size = keyof typeof SIZES;

type Status = {
  state: "idle" | "running" | "exited" | "failed";
  started_ms?: number;
  ended_ms?: number;
  error?: string;
  instance?: string;
};

const MAX_JOB_MS = 6 * 60 * 60 * 1000;

class Runner extends DurableObject<Env> {
  async begin(jit: string, instance?: string): Promise<Status> {
    const c = this.ctx.container!;
    if (c.running) return this.status();
    const st: Status = { state: "running", started_ms: Date.now(), instance };
    await this.ctx.storage.put("status", st);
    // Keep the container alive while the DO is idle (the job may take hours).
    await c.setInactivityTimeout(MAX_JOB_MS);
    c.start({
      enableInternet: true,
      entrypoint: ["/home/runner/rgha-entrypoint.sh"],
      env: { ACTIONS_RUNNER_INPUT_JITCONFIG: jit },
    });
    this.ctx.waitUntil(
      c.monitor().then(
        () => this.finish({ state: "exited" }),
        (e) => this.finish({ state: "failed", error: String(e) }),
      ),
    );
    return st;
  }

  async finish(patch: Partial<Status>) {
    const st = ((await this.ctx.storage.get<Status>("status")) ?? { state: "idle" }) as Status;
    await this.ctx.storage.put("status", { ...st, ...patch, ended_ms: Date.now() });
  }

  async status(): Promise<Status> {
    const st = (await this.ctx.storage.get<Status>("status")) ?? { state: "idle" };
    if (st.state === "running" && !this.ctx.container!.running) return { ...st, state: "exited" };
    return st;
  }

  async end(): Promise<Status> {
    if (this.ctx.container!.running) await this.ctx.container!.destroy("stopped by rgha");
    await this.finish({ state: "exited" });
    return this.status();
  }
}

function authorized(req: Request, env: Env): boolean {
  const got = req.headers.get("authorization") ?? "";
  const want = `Bearer ${env.RGHA_TOKEN}`;
  if (!env.RGHA_TOKEN || got.length !== want.length) return false;
  let diff = 0;
  for (let i = 0; i < got.length; i++) diff |= got.charCodeAt(i) ^ want.charCodeAt(i);
  return diff === 0;
}

export class RunnerBasic extends Runner {}
export class RunnerStandard1 extends Runner {}
export class RunnerStandard2 extends Runner {}
export class RunnerStandard4 extends Runner {}

export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    if (!authorized(req, env)) return new Response("unauthorized", { status: 401 });
    const m = new URL(req.url).pathname.match(/^\/runners\/([A-Za-z0-9._-]{1,64})$/);
    if (!m) return new Response("not found", { status: 404 });
    const size = (new URL(req.url).searchParams.get("instance") ?? "basic") as Size;
    if (!(size in SIZES)) return new Response("unknown instance", { status: 400 });
    const ns = SIZES[size](env);
    const stub = ns.get(ns.idFromName(m[1]));
    switch (req.method) {
      case "POST": {
        const body = (await req.json()) as { jit?: string };
        if (!body.jit) return new Response("jit required", { status: 400 });
        return Response.json(await stub.begin(body.jit, size));
      }
      case "GET":
        return Response.json(await stub.status());
      case "DELETE":
        return Response.json(await stub.end());
      default:
        return new Response("method not allowed", { status: 405 });
    }
  },
} satisfies ExportedHandler<Env>;
