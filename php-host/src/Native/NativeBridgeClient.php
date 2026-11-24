<?php

declare(strict_types=1);

namespace Whity\Native;

/**
 * One plugin's way to the hardware: calls the Rust native bridge on loopback
 * with the handle the host minted for that plugin (crates/drivers/src/bridge.rs).
 *
 * Over the curl extension, not PHP streams: the sandbox ini turns
 * allow_url_fopen off for every plugin, and this client must still work.
 * Loopback only, no redirects, short timeouts.
 */
final class NativeBridgeClient
{
    public function __construct(
        private readonly string $baseUrl,
        private readonly string $handle,
        private readonly int $timeoutSeconds = 30,
    ) {
    }

    /**
     * Run one capability, for example `printer.print` with `['text' => "..."]`,
     * and return the driver's result.
     *
     * @param array<string, mixed> $args
     * @return array<string, mixed>
     * @throws NativeBridgeRefusedException When the bridge says no: the plugin
     *         did not declare the capability, the device has it off or on
     *         "ask first", no driver offers it, or the arguments are wrong.
     * @throws NativeBridgeUnavailableException When the bridge cannot be reached.
     */
    public function call(string $capability, array $args = []): array
    {
        if (preg_match('/^[a-z][a-z0-9-]*\.[a-z][a-z0-9_]*$/', $capability) !== 1) {
            throw new NativeBridgeRefusedException('invalid_capability', "Not a capability name: {$capability}", 400);
        }
        $decoded = NativeBridgeHttp::postJson(
            $this->baseUrl . '/native/' . $capability,
            ['X-Whity-Handle: ' . $this->handle],
            $args === [] ? new \stdClass() : $args,
            $this->timeoutSeconds,
        );
        $result = $decoded['result'] ?? [];

        return is_array($result) ? $result : ['value' => $result];
    }
}
