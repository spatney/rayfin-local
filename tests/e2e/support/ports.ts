import net from 'node:net';

const LOOPBACK = ['127.0.0.1', '::1'];
/** Below Windows' (49152+) and Linux's (32768+) ephemeral ranges, so outgoing connections don't take them. */
const RANGE = { min: 20_000, max: 31_999 };
const handedOut = new Set<number>();

function canListen(port: number, host: string): Promise<boolean> {
  return new Promise((resolve) => {
    const server = net.createServer();
    server.unref();
    server.once('error', (error: NodeJS.ErrnoException) => {
      // A machine without IPv6 can't have anything listening on it either.
      resolve(error.code === 'EADDRNOTAVAIL' || error.code === 'EAFNOSUPPORT');
    });
    server.listen({ port, host, exclusive: true }, () => server.close(() => resolve(true)));
  });
}

/** Whether something accepts connections on `host:port`. */
function accepts(port: number, host: string, timeoutMs = 1_000): Promise<boolean> {
  return new Promise((resolve) => {
    const socket = net.connect({ port, host });
    const done = (result: boolean) => {
      socket.destroy();
      resolve(result);
    };
    socket.setTimeout(timeoutMs, () => done(false));
    socket.once('connect', () => done(true));
    socket.once('error', () => done(false));
  });
}

/** Whether nothing listens on `port` on any loopback or wildcard address. */
export async function isPortFree(port: number): Promise<boolean> {
  for (const host of [...LOOPBACK, '0.0.0.0', '::']) {
    if (!(await canListen(port, host))) return false;
  }
  return true;
}

export async function isListening(port: number): Promise<boolean> {
  for (const host of LOOPBACK) {
    if (await accepts(port, host)) return true;
  }
  return false;
}

/** `count` distinct ports, free on every loopback address, never handed out before in this process. */
export async function freePorts(count: number): Promise<number[]> {
  const ports: number[] = [];
  for (let attempt = 0; ports.length < count; attempt++) {
    if (attempt > 500) throw new Error(`Found only ${ports.length} of ${count} free ports.`);
    const port = RANGE.min + Math.floor(Math.random() * (RANGE.max - RANGE.min));
    if (handedOut.has(port) || !(await isPortFree(port))) continue;
    handedOut.add(port);
    ports.push(port);
  }
  return ports;
}

/** Waits until nothing accepts connections on `ports`; throws naming the ports still in use. */
export async function waitForPortsClosed(ports: number[], timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const busy: number[] = [];
    for (const port of ports) if (await isListening(port)) busy.push(port);
    if (busy.length === 0) return;
    if (Date.now() > deadline) {
      throw new Error(`Port(s) ${busy.join(', ')} still accept connections ${timeoutMs} ms after stopping.`);
    }
    await sleep(200);
  }
}

export function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
