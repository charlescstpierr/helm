# Vérifications et PR GitHub

> Pour les agents : exécuter les tâches par responsabilités séparées, avec tests de régression puis revue de la branche complète.

**Objectif :** une carte produit des vérifications exécutées par Helm, une PR prête à relire et une action de fusion confirmée par GitHub.

**Architecture :** Rust/Axum/SQLite existants, client `gh` installé par l'utilisateur, aucune dépendance front. Les commandes sont configurées localement, exécutées dans le worktree et rattachées à son commit. Les instantanés GitHub sont persistés par carte. Les actions de fusion relisent GitHub et vérifient le commit attendu.

**Périmètre approuvé :** premier lot « PR GitHub + vérifications automatiques » de la proposition acceptée dans la conversation. La reprise de session, Codex et les notifications feront l'objet d'autres lots.

## Contraintes

- Rust ≥ 1.85, interface et documentation en français, code en anglais.
- Pas de Node, CDN, styles ou scripts inline ; formulaires utilisables sans JavaScript.
- Migrations append-only. GitHub désactivé par défaut ; les configurations existantes restent valides.
- Les tests n'appellent ni Claude ni GitHub réels : faux exécutables et dépôts temporaires.
- Les commandes, sorties et codes de retour des contrôles restent consultables. Aucune commande configurée signifie « Non exécutées », jamais « Réussies ».
- Timeout, sortie bornée et arrêt du groupe de processus pour les commandes externes.
- Une vérification réussie porte sur un commit précis et un arbre propre. Un changement du commit ou des fichiers pendant les contrôles bloque la publication.
- La fusion est explicite, vise le commit vérifié, respecte les protections GitHub et ne force rien. Une carte ne passe à Terminé qu'après confirmation de la fusion.

## Interfaces communes

- `config::ChecksConfig { commands: Vec<String>, timeout: Duration }`, valeur par défaut sans commande et timeout de 10 minutes ; `[checks] commands, timeout_minutes`.
- `config::GithubConfig { repository: String, command: PathBuf }` ; `Config.github: Option<GithubConfig>`, activé par `[github] repository = "owner/repo"`, commande par défaut `gh`.
- `process::capture(program: &Path, args: &[String], cwd: &Path, stdin: Option<&str>, timeout: Duration) -> Result<CommandOutput, String>`. `CommandOutput` expose `stdout`, `stderr`, `exit_code: Option<i32>`, `timed_out: bool`, `success()`.
- `checks::VerificationStatus` : Running, Passed, Failed, Skipped, Interrupted ; `CheckStatus` : Pending, Running, Passed, Failed, Interrupted.
- `checks::Verification { commit_sha, status, checks: Vec<CheckResult> }` ; `CheckResult { position: i64, command, status, stdout, stderr, exit_code: Option<i32>, error: Option<String> }`.
- Stockage synchrone : `checks::start(conn, run_id, commit_sha, commands)`, `start_check(conn, run_id, position)`, `finish_check(conn, run_id, position, &CommandOutput)`, `finish(conn, run_id, status)`, `get(conn, run_id) -> Option<Verification>`, `interrupt_unfinished(conn)` ; toutes renvoient `store::Result`.
- Exécution d'un contrôle : `checks::execute(command, cwd, timeout) -> Result<CommandOutput, String>` via `sh -c`, sans interpolation supplémentaire.
- `github::Github::new(config: GithubConfig)` ; méthodes async `ensure(cwd, branch, base, title, body)`, `refresh(cwd, number: i64)`, `merge(cwd, number, expected_head, expected_base)`, retournant `Result<PullRequest, String>`.
- `github::PullRequest` sérialisable expose number, url, state, draft, head_sha, head_branch, base_branch, checks ; méthodes `state_label`, `checks_label`, `merge_blocker() -> Option<&str>`. `PullRequestState` : Open, Closed, Merged ; `CiStatus` : None, Pending, Passed, Failed, Unknown.
- `delivery::Delivery::new(db, changes, gate, repo: Option<PathBuf>, github: Option<GithubConfig>)`, `enabled()`, async `publish(run_id, expected_head)`, `refresh(card_id)`, `merge(card_id)` ; les erreurs sont des chaînes présentables.
- `delivery::get(conn, card_id) -> store::Result<Option<CardPullRequest>>` avec `CardPullRequest { run_id, repository, expected_base, pr: PullRequest, refreshed_at: i64, error: Option<String> }`.

## Tâche 1 : commandes et résultats de vérification

Fichiers : `src/process.rs`, `src/checks.rs`, `src/config.rs`, `src/db.rs`, `migrations/0005_delivery.sql`.

- [x] Tester configuration par défaut, paramètres invalides, succès/échec/timeout/sorties volumineuses/arrêt des enfants et persistance des contrôles non exécutés.
- [x] Implémenter les interfaces communes. La migration ajoute les tables `run_verifications`, `run_checks` et `card_pull_requests` sans modifier les anciennes.
- [x] Prévoir dans `card_pull_requests` : card_id PK cascade cards, run_id FK cascade agent_runs, repository TEXT, expected_base TEXT NOT NULL, snapshot TEXT JSON, refreshed_at INTEGER, error TEXT nullable, unicité repository + numéro via une colonne number INTEGER.
- [x] Vérifier les tests ciblés.

## Tâche 2 : client GitHub

Fichiers : `src/github.rs`, faux client dans `tests/fixtures/`.

- [x] Tester création d'une PR prête, réutilisation de la PR ouverte, erreurs du CLI et décodage CI.
- [x] Implémenter `ensure`, `refresh`, `merge` via `gh --repo` explicite ; le corps passe par stdin avec `--body-file -`.
- [x] Refuser la fusion si draft, CI en cours/échouée/inconnue, état non fusionnable, revue bloquante ou commit différent ; utiliser `--match-head-commit` et confirmer MERGED ensuite.
- [x] Tester absence de contrôles distincte du succès, données GitHub malformées, changement du commit et refus de fusion.

## Tâche 3 : orchestration et livraison

Fichiers : `src/delivery.rs`, `src/supervisor.rs`, `src/git.rs`, `src/main.rs`.

- [x] Exécuter et enregistrer chaque commande avant le push ; annulation et arrêt marquent les contrôles interrompus.
- [x] Vérifier branche, commit et propreté avant/après ; publier le commit exact quand les vérifications ou GitHub sont configurés.
- [x] Créer/réutiliser la PR après le push ; conserver une erreur et une action de réessai si GitHub est indisponible après le push.
- [x] Sérialiser les opérations de livraison par carte avec la mise en file ; refuser une fusion pendant une exécution active ou sur une vérification périmée.
- [x] Enregistrer les instantanés GitHub, publier les changements SSE, synchroniser Terminé seulement si la dernière livraison est fusionnée et aucune exécution n'est active.
- [x] Tester les parcours contrôle échoué → aucun push, contrôle réussi → PR, erreur GitHub → réessai, CI bloquante et fusion confirmée.

## Tâche 4 : interface, documentation et validation

Fichiers : `src/routes.rs`, `templates/_activity.html`, assets si nécessaires, README et exemple de configuration.

- [x] Afficher commit vérifié, chaque commande, état, sorties et code retour ; lien PR, état et CI, dernière actualisation.
- [x] Formulaires POST pour publier/réessayer, actualiser et fusionner ; même fonctionnement en page complète et en dialogue.
- [x] Tests routes incluant CSRF, absence de GitHub, conflits, erreurs, redirections sans JavaScript et rendu échappé.
- [x] Documentation française et exemple avec `cargo fmt --check`, Clippy et tests.
- [x] `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings`, `cargo test --locked`, revue indépendante.

## Points de revue

Commit changé pendant un contrôle ; PR dont le commit a changé ; doublon de publication ; fusion concurrente avec une relance ; contrôles inconnus/absents et erreurs réseau ne doivent jamais être assimilés à un succès.

## Validation effectuée

- Formatage et Clippy sans avertissement ; 223 tests réussis.
- Revue indépendante des parcours de publication, des annulations, des réservations par carte et de la fusion.
- Test HTTP du binaire local, sans JavaScript : carte → deux contrôles → push du SHA vérifié → PR prête → fusion explicite → Terminé. Dépôt temporaire et faux CLI uniquement.
