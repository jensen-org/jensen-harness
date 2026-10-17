rule private_key {
    strings:
        $pem = /-----BEGIN ((RSA|EC|DSA|OPENSSH|PGP|ENCRYPTED) )?PRIVATE KEY( BLOCK)?-----/
    condition:
        $pem
}

rule aws_access_key {
    strings:
        $id = /\b(AKIA|ASIA|AGPA|AIDA|AROA|ANPA)[0-9A-Z]{16}\b/
    condition:
        $id
}

rule github_token {
    strings:
        $classic = /\b(ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,}\b/
        $fine = /\bgithub_pat_[A-Za-z0-9_]{50,}\b/
    condition:
        any of them
}

rule gitlab_token {
    strings:
        $pat = /\bglpat-[A-Za-z0-9_\-]{20,}/
    condition:
        $pat
}

rule slack_token {
    strings:
        $token = /\bxox[baprs]-[A-Za-z0-9-]{10,}/
    condition:
        $token
}

rule model_provider_key {
    strings:
        $anthropic = /\bsk-ant-[A-Za-z0-9_\-]{20,}/
        $openai = /\bsk-(proj-)?[A-Za-z0-9_\-]{40,}/
    condition:
        any of them
}

rule credentialed_url {
    strings:
        $url = /[a-z][a-z0-9+.\-]*:\/\/[^\s\/:@]+:[^\s\/@]{3,}@[^\s\/]+/
    condition:
        $url
}
