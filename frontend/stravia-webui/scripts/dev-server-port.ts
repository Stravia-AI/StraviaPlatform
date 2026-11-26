import { createServer as createNetServer } from 'node:net'

export const DEFAULT_BACKEND_PORT = 23471

export function parseBackendPort(rawPort: string): number {
  const port = Number(rawPort)
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error(`STRAVIA_PORT must be an integer between 1 and 65535, received "${rawPort}"`)
  }
  return port
}

function canBindPort(host: string, port: number): Promise<boolean> {
  const { promise, resolve, reject } = Promise.withResolvers<boolean>()
  const server = createNetServer()
  server.unref()
  server.once('error', (error: NodeJS.ErrnoException) => {
    if (error.code === 'EADDRINUSE') {
      resolve(false)
      return
    }
    reject(error)
  })
  server.listen({ host, port, exclusive: true }, () => {
    server.close((closeError) => {
      if (closeError) {
        reject(closeError)
        return
      }
      resolve(true)
    })
  })
  return promise
}

export async function resolveAvailableBackendPort(
  host: string,
  requestedPort: number,
  maxPort = 65535,
): Promise<number> {
  if (!Number.isInteger(requestedPort) || requestedPort < 1 || requestedPort > maxPort) {
    throw new Error(`STRAVIA_PORT must be an integer between 1 and ${maxPort}, received "${requestedPort}"`)
  }

  for (let port = requestedPort; port <= maxPort; port += 1) {
    if (await canBindPort(host, port)) {
      return port
    }
  }

  throw new Error(`No available backend port found on ${host} starting from ${requestedPort}`)
}
