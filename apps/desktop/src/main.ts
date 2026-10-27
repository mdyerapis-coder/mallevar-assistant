import "./ironbark.css";
// Ironbark Assistant desktop entry. Wires the typed CoreClient to the
// projection and the navigation rail, and keeps a durable event subscription
// alive. The webview is read-only: it displays state, it never mutates it.
import "./styles.css";
import { CoreClient, type EventEnvelope } from "./core-client";
import {
  applyDoctorError,
  applyDoctorOk,
  applyEvent,
  applyProjects,
  applyProjectsError,
  applyStatus,
  coreState,
  initialProjection,
  render,
  type Projection,
} from "./projection";
import { renderNavigation, setNavigateHandler, type ViewName } from "./navigation";

const client = new CoreClient();
let proj: Projection = initialProjection();
let view: ViewName = "status";
let root: HTMLElement;

function refresh(): void {
  const nav = document.getElementById("nav") as HTMLElement;
  renderNavigation(nav, view);
  render(proj, root);
}

function setView(name: ViewName): void {
  view = name;
  refresh();
}

setNavigateHandler(setView);

async function loadInitial(): Promise<void> {
  const title = (text: string): void => { document.title = `Ironbark — ${text}`; };
  title("status…");
  try {
    proj = applyStatus(proj, await client.status());
  } catch (err) {
    proj = coreState(proj, "error");
    title(`status failed: ${String(err)}`);
  }
  title("doctor…");
  try {
    proj = applyDoctorOk(proj, await client.doctor());
  } catch (err) {
    proj = applyDoctorError(proj, String(err));
    title(`doctor failed: ${String(err)}`);
  }
  title("projects…");
  try {
    proj = applyProjects(proj, await client.projectList());
  } catch (err) {
    proj = applyProjectsError(proj, String(err));
    title(`projects failed: ${String(err)}`);
  }
  title("ready");
  refresh();
}

async function start(): Promise<void> {
  root = document.getElementById("app") as HTMLElement;
  setNavigateHandler(setView);
  await loadInitial();

  let last = proj.lastSeq;
  try {
    await client.subscribe(
      last,
      (e: EventEnvelope) => {
        proj = applyEvent(proj, e);
        refresh();
      },
      (state) => {
        proj = coreState(proj, state);
        refresh();
      },
    );
  } catch {
    proj = coreState(proj, "error");
    refresh();
  }

  // Refresh the project list after any state-change event that may have
  // created one; the status pane stays live via the event feed.
  const timer = setInterval(() => {
    client
      .projectList()
      .then((projects) => {
        proj = applyProjects(proj, projects);
        refresh();
      })
      .catch(() => {});
  }, 15_000);
  window.addEventListener("pagehide", () => clearInterval(timer));
}

void start();
