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
     * @throws NativeBridgeRefusedException When the call did not run: the
     *         plugin did not declare the capability, the device has it off,
     *         no driver offers it, or the arguments are wrong. With code
     *         `ask_first` the call became a proposal: show the person it is
     *         waiting, and ask {@see proposal()} for the outcome.
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

    /**
     * The outcome of a call that became a proposal. `state` is `waiting`,
     * `done` (with `result`), `failed` (with `error`), `declined` or `expired`.
     *
     * @return array<string, mixed>
     * @throws NativeBridgeRefusedException With `not_found` when the id is not
     *         one of this plugin's proposals.
     * @throws NativeBridgeUnavailableException
     */
    public function proposal(string $proposalId): array
    {
        if (preg_match('/^[a-f0-9]{64}$/', $proposalId) !== 1) {
            throw new NativeBridgeRefusedException('not_found', 'Not a proposal id', 404);
        }
        $decoded = NativeBridgeHttp::getJson(
            $this->baseUrl . '/native/proposals/' . $proposalId,
            ['X-Whity-Handle: ' . $this->handle],
            $this->timeoutSeconds,
        );
        $outcome = $decoded['outcome'] ?? [];

        return is_array($outcome) ? $outcome : ['state' => 'waiting'];
    }
}
