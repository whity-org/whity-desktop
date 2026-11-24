<?php
// Verifies attestation-vector.json the way whity-core's PluginTrust::verified()
// and checkAttestation() do, with PHP's sodium: the same message construction
// (format . "
" . payload), the same envelope fields, the same reasons.
// Run: php -d extension=sodium verify-vector.php attestation-vector.json
declare(strict_types=1);

$vector = json_decode((string) file_get_contents($argv[1]), true, 16, JSON_THROW_ON_ERROR);
$keys = [$vector['publicKey']];
$now = (int) $vector['now'];

function verified(array $keys, string $envelope, string $format): ?array
{
    $decoded = json_decode($envelope, true);
    if (!is_array($decoded) || !is_string($decoded['key'] ?? null)) {
        return null;
    }
    $key = null;
    foreach ($keys as $pinned) {
        if (hash_equals($pinned, $decoded['key'])) {
            $raw = base64_decode($pinned, true);
            $key = $raw === false || strlen($raw) !== SODIUM_CRYPTO_SIGN_PUBLICKEYBYTES ? null : $raw;
        }
    }
    $payload = is_string($decoded['payload'] ?? null) ? base64_decode($decoded['payload'], true) : false;
    $signature = is_string($decoded['signature'] ?? null) ? base64_decode($decoded['signature'], true) : false;
    if ($key === null || $payload === false || $signature === false || strlen($signature) !== SODIUM_CRYPTO_SIGN_BYTES) {
        return null;
    }
    if (!sodium_crypto_sign_verify_detached($signature, $format . "\n" . $payload, $key)) {
        return null;
    }
    $body = json_decode($payload, true);
    return is_array($body) && ($body['format'] ?? null) === $format ? $body : null;
}

$list = verified($keys, $vector['revocations'], 'whity.revocations/1');
if ($list === null) {
    fwrite(STDERR, "revocation list: bad_signature\n");
    exit(1);
}
$stale = (int) $list['expires_at'] <= $now;

function check(array $keys, array $list, bool $stale, int $now, string $envelope, string $plugin, string $version, string $sha): string
{
    $payload = verified($keys, $envelope, 'whity.attestation/1');
    if ($payload === null) {
        return 'bad_signature';
    }
    if (($payload['plugin'] ?? null) !== $plugin || ($payload['version'] ?? null) !== $version || !is_string($payload['sha256'] ?? null) || !hash_equals($payload['sha256'], $sha)) {
        return 'mismatch';
    }
    if (!is_int($payload['expires_at'] ?? null) || $payload['expires_at'] <= $now) {
        return 'expired';
    }
    if ($stale) {
        return 'list_stale';
    }
    foreach ($list['revoked'] as $entry) {
        if (($entry['plugin'] ?? null) === $plugin && (($entry['version'] ?? null) === null || $entry['version'] === $version)) {
            return 'revoked';
        }
    }
    return 'verified';
}

$e = $vector['expect'];
$results = [
    'good' => check($keys, $list, $stale, $now, $vector['attestation'], $e['plugin'], $e['version'], $e['sha256']),
    'revoked' => check($keys, $list, $stale, $now, $vector['revokedAttestation'], 'RevokedPlugin', '1.0.0', $e['sha256']),
    'wrongVersion' => check($keys, $list, $stale, $now, $vector['attestation'], $e['plugin'], '9.9.9', $e['sha256']),
    'forgedKey' => check([base64_encode(sodium_crypto_sign_publickey(sodium_crypto_sign_keypair()))], $list, $stale, $now, $vector['attestation'], $e['plugin'], $e['version'], $e['sha256']),
];
echo json_encode($results), "\n";
$ok = $results === ['good' => 'verified', 'revoked' => 'revoked', 'wrongVersion' => 'mismatch', 'forgedKey' => 'bad_signature'];
echo $ok ? "PHP agrees with Rust\n" : "PHP DISAGREES\n";
exit($ok ? 0 : 1);
