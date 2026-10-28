import type { BrowserContext, Page } from '@playwright/test';

/*
 * Console errors, uncaught exceptions, failed requests and HTTP errors of every page in a browser
 * context (the app and its sign-in popups), so tests can check that nothing unexpected went wrong.
 */

export interface LogEntry {
  kind: 'console' | 'pageerror' | 'requestfailed' | 'response';
  /** The page's URL path, e.g. `/` (the app) or `/.rayfin-local/portal/secureItemEmbed`. */
  page: string;
  /** The request or resource URL, if any. */
  url?: string;
  status?: number;
  text: string;
}

/** Matches the entry's `"<kind> <status> <url> <text>"`, e.g. `/^response 500 .*\/functions\/fail\//`. */
export type Allowed = RegExp;

export class BrowserLog {
  readonly #entries: LogEntry[] = [];
  /** Requests cancelled without a response, by page: benign once that page has closed itself. */
  readonly #aborted = new Map<LogEntry, Page>();

  constructor(context: BrowserContext) {
    for (const page of context.pages()) this.#watch(page);
    context.on('page', (page) => this.#watch(page));
  }

  /** The position to check from with `since`. */
  mark(): number {
    return this.#entries.length;
  }

  since(mark: number): LogEntry[] {
    return this.#entries.slice(mark);
  }

  /**
   * The entries since `mark` that no pattern in `allowed` matches, leaving out requests that were
   * cancelled because their page closed (the sign-in popup closes itself once it is done).
   */
  unexpected(mark: number, allowed: Allowed[]): LogEntry[] {
    return this.since(mark).filter(
      (entry) =>
        !this.#aborted.get(entry)?.isClosed() && !allowed.some((pattern) => pattern.test(describe(entry))),
    );
  }

  #watch(page: Page): void {
    const where = () => {
      try {
        return new URL(page.url()).pathname;
      } catch {
        return page.url();
      }
    };
    page.on('console', (message) => {
      if (message.type() !== 'error') return;
      this.#entries.push({ kind: 'console', page: where(), url: message.location().url || undefined, text: message.text() });
    });
    page.on('pageerror', (error) => {
      this.#entries.push({ kind: 'pageerror', page: where(), text: `${error.name}: ${error.message}` });
    });
    page.on('requestfailed', async (request) => {
      const at = where();
      const errorText = request.failure()?.errorText ?? 'failed';
      // Cancelled after its response arrived (e.g. a body the SDK doesn't read): not a failure;
      // an HTTP error status is logged as a response.
      if (errorText === 'net::ERR_ABORTED' && (await request.response().catch(() => null))) return;
      const entry: LogEntry = {
        kind: 'requestfailed',
        page: at,
        url: request.url(),
        text: `${request.method()} ${errorText}`,
      };
      this.#entries.push(entry);
      if (errorText === 'net::ERR_ABORTED') this.#aborted.set(entry, page);
    });
    page.on('response', (response) => {
      if (response.status() < 400) return;
      const request = response.request();
      this.#entries.push({
        kind: 'response',
        page: where(),
        url: response.url(),
        status: response.status(),
        text: `${request.method()} ${response.status()} ${response.statusText()}`,
      });
    });
  }
}

export function describe(entry: LogEntry): string {
  return [entry.kind, entry.status, entry.url, entry.text].filter((part) => part !== undefined).join(' ');
}
