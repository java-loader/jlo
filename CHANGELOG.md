# Changelog

## [0.4.0](https://github.com/java-loader/jlo/compare/jlo-bin-v0.3.0...jlo-bin-v0.4.0) (2026-09-21)


### ⚠ BREAKING CHANGES

* **selfupdate:** `jlo selfupdate` no longer runs install.sh. Upgrading from 0.3.x still works through the old wrapper's curl path; restart the shell once afterwards, as README's new Upgrading section describes.
* **shell:** the release tarball no longer contains jlo-init.sh or jlo-autoload.sh, and $JLO_HOME/bin/jlo-init.sh is replaced by jlo-init.{bash,zsh}. The three profile lines are unchanged.
* **cli:** drop the 'version' transition shim

### Features

* **cli:** drop the 'version' transition shim ([39f4c8f](https://github.com/java-loader/jlo/commit/39f4c8fb4e5c4beec4cc34317faac7153c948a02))
* **current:** add 'jlo current', which says what is active and why ([08159af](https://github.com/java-loader/jlo/commit/08159af688f74ef98901b17456cb2c31abcda950))
* **env:** add 'jlo env --verbose', which says what it set and why ([5b1ad63](https://github.com/java-loader/jlo/commit/5b1ad638f80065409bcdbd979ad111a89e34311b))
* **install:** generate profile entry files instead of a twelve-line paste ([2ed0998](https://github.com/java-loader/jlo/commit/2ed0998ed904e8b106e8ce43ee2b9542cdc82daa))
* **selfupdate:** update J'Lo from the binary, not from curl | bash ([f582780](https://github.com/java-loader/jlo/commit/f582780a0b482274f4e5fb945089ea43780a5a47))
* **shell:** generate the shell layout from the binary ([3220955](https://github.com/java-loader/jlo/commit/3220955c0dcbcecadcc1b5e4cf2f9a065c9b1729))


### Bug Fixes

* **adoptium:** require API path fields to be plain names ([61f2efd](https://github.com/java-loader/jlo/commit/61f2efd08e03ebd51e05a477440f4ca3311a30f1))
* **env:** stop 'jlo env' crashing when the reader closes the pipe ([06f8ebd](https://github.com/java-loader/jlo/commit/06f8ebd9ca5acef175ce0c8a49a8dc946760e955))
* **install:** keep published installs from loading the old wrapper forever ([ae091b1](https://github.com/java-loader/jlo/commit/ae091b194a1353665b432c9da22d56984625cfa2))
* **install:** stop handing tar a path it reinterprets ([6e00601](https://github.com/java-loader/jlo/commit/6e006012fb872d7d3ba4a0e1ad83f40a84adfb3d))
* **selfupdate:** report failure instead of reporting success ([bace452](https://github.com/java-loader/jlo/commit/bace45206289dd78bca4baf1175a393f3ae79ed8))


### Performance Improvements

* **completions:** autoload the zsh completion from $fpath ([4ba3963](https://github.com/java-loader/jlo/commit/4ba39634a95303573074ca2c532be62940378612))

## [0.3.0](https://github.com/java-loader/jlo/compare/jlo-bin-v0.2.0...jlo-bin-v0.3.0) (2026-09-21)


### ⚠ BREAKING CHANGES

* **env:** jlo env and jlo use now emit export lines quoted with single quotes rather than double quotes. Anything parsing that output must expect `export PATH='...'`.
* **shell:** jlo env and jlo use now exit with the binary's status instead of always 0. jlo_after_cd already ended in an explicit `return 0`; the fresh-shell call at the tail of jlo-autoload.sh gained a `|| :` so a failing --offline lookup cannot abort a profile under `set -e`.
* **env:** add --offline, and stop cd from downloading a JDK
* **list:** `jlo list` rows are indented by a four-character gutter and no longer start with the major version. `outdated (<version>)` is replaced by `update` on the offered build's row, the parenthetical being redundant once the installed build has a row of its own. The `TIP:` line now covers pruning as well as updating and is capped at one line.
* **cli:** jlo clean is now jlo prune, with no alias left behind - the old name is a usage error. The pair remove (the versions you chose) / prune (whatever a rule leaves over) is a familiar division where remove / clean was not, and clean promised regenerable build output - cargo clean, gradle clean
    - while this command deletes real JDKs. Keeping the old name alive, even
    unadvertised, would have kept that misreading available to exactly the people
    the rename is meant to protect. The internal names moved with it
    (JdkStore::prune, PruneReport, ui::prune_report, cmd_prune) so the code does
    not argue with its own help. The jlo update hint now names prune.
* **cli:** jlo version no longer works; use jlo --version or jlo -V.

### Features

* add 'jlo home' and 'jlo exec' commands ([b3b89ac](https://github.com/java-loader/jlo/commit/b3b89ac26c94259b0dccd91b8649d2212e66a3e3))
* add 'jlo list' showing available and installed JDKs ([f54ed82](https://github.com/java-loader/jlo/commit/f54ed82fa7d192496ec18c9a789e72258d807ab1))
* **cli:** add 'jlo completions' for bash, zsh and fish ([810839d](https://github.com/java-loader/jlo/commit/810839db6aa2242a3765dd06bccbe27b1663ecba))
* **cli:** add remove and home --offline, rename clean to prune ([a7036f8](https://github.com/java-loader/jlo/commit/a7036f859e1ddcc89199d031a4514f171d170ce4))
* **cli:** collapse install output into one live line ([66ce995](https://github.com/java-loader/jlo/commit/66ce99564b0fb3156a6bfe50dc9bfbc53307c846))
* **cli:** generate help from the command definitions ([f3b9fdc](https://github.com/java-loader/jlo/commit/f3b9fdc2fb74503ddfc00cacc84088769094d1b3))
* **cli:** remove the version subcommand, keep -V/--version ([aeb43f6](https://github.com/java-loader/jlo/commit/aeb43f6e0a8d6249d4fe3b4a5bff72625e2961ef))
* **deps:** replace reqwest with ureq, bump remaining major versions ([0258b23](https://github.com/java-loader/jlo/commit/0258b2331009d641683fc6b2b4c152dd212d0d5d))
* **env:** add --offline, and stop cd from downloading a JDK ([06d6ec6](https://github.com/java-loader/jlo/commit/06d6ec6667e630d05ca70e11fbac29d0ff46eea8))
* **env:** warn when jlo env's exports go nowhere ([c207aaa](https://github.com/java-loader/jlo/commit/c207aaa3df670b4017f6e685d7047c95847bb652))
* install 'jlo' on PATH and document non-interactive usage ([aeb75a8](https://github.com/java-loader/jlo/commit/aeb75a8037c061df286b8743b7b581e8f0620616))
* **install:** generate and wire up shell completions ([59f0433](https://github.com/java-loader/jlo/commit/59f04334c8a83aeaf29a19286c7aac77bcf35c23))
* **list:** give every installed build its own row, and mark the active one ([d14c0da](https://github.com/java-loader/jlo/commit/d14c0da47abf0fb9de187936ff9a4abb0e19bd78))
* **update:** make --all a flag instead of a magic "all" argument ([3ced004](https://github.com/java-loader/jlo/commit/3ced004b77c82250a89c43d781461a03fbd8fe26))
* **update:** point at jlo clean when an update supersedes a minor ([003ae9a](https://github.com/java-loader/jlo/commit/003ae9a4b3e1608efd2c9ced7499692fa241aa96))


### Bug Fixes

* check HTTP status in latest_major and download, propagate read errors ([afe0b0b](https://github.com/java-loader/jlo/commit/afe0b0b977caf8fd1ed222948b7e51b85f5df13f))
* **cli:** colour errors, warnings and hints ([3c49cb1](https://github.com/java-loader/jlo/commit/3c49cb1795a3d8de949e4557ce6e0b2655059d26))
* **cli:** fix exec separator/help handling, alias and wording from review ([a895ea2](https://github.com/java-loader/jlo/commit/a895ea2452d45028ddeb469f4d7ba97d15b5c670))
* **cli:** give diagnostics one voice ([67babbb](https://github.com/java-loader/jlo/commit/67babbbc60bbc68314006446bb8a039a5dfa24a0))
* **cli:** shell-guard completion snippet, selfupdate transition shim, help polish ([ad8ea5c](https://github.com/java-loader/jlo/commit/ad8ea5c092639113ab83ae7844c1826b4e291d17))
* **cli:** stop leaking the sing easter egg via completions and typos ([0654a71](https://github.com/java-loader/jlo/commit/0654a71254d17f98518fc759aaac7afbae8fa39e))
* **config:** match versions on the parsed major, and name the accepted form ([4d9bbcc](https://github.com/java-loader/jlo/commit/4d9bbccc23c770ea1ef38f8a48548a9b2b6c383b))
* **config:** resolve .jlorc from parent directories ([3bb53e6](https://github.com/java-loader/jlo/commit/3bb53e6bee4d99f6175c1809b8bbd17748f980a3))
* **config:** resolve default.jlorc under $HOME/.jlo, not bare $HOME ([487d081](https://github.com/java-loader/jlo/commit/487d081d20af98ba781a716dc5b2458136a41bb4))
* **deps:** patch two advisories, trim zip codecs, enforce lints in CI ([20c6120](https://github.com/java-loader/jlo/commit/20c6120f38a908da8c0414b462f1214278a7b215))
* **env:** shell-quote the values jlo env exports ([0417219](https://github.com/java-loader/jlo/commit/0417219f6b0c3effcda65d5808e9c7403c38f3ca))
* filter PATH on the JDK install dir, not JLO_HOME ([a00372f](https://github.com/java-loader/jlo/commit/a00372f5eaf850ab966ed9c7315da94b748360bf))
* harden install symlink and jlo exec per review ([f477f11](https://github.com/java-loader/jlo/commit/f477f11c2d2b03717a0f157ecc7bb5358a8aafe8))
* **install:** correct zsh completion setup and drop fish ([8ced9a9](https://github.com/java-loader/jlo/commit/8ced9a90b5f86820efa425f1bbd9740bfa6e20a6))
* **install:** make zsh completion instructions order-independent ([95c964e](https://github.com/java-loader/jlo/commit/95c964eda6ec3eeff7b2a2a6eb98f98174967771))
* make bash autoload hook registration idempotent and array-safe ([2751b0d](https://github.com/java-loader/jlo/commit/2751b0d2412cc607770cb96eee478067d4ed9d33))
* **shell:** make jlo env work under macOS's bash 3.2 ([bfda43b](https://github.com/java-loader/jlo/commit/bfda43bc6fa4955e6de021708439c72584f18b62))
* **shell:** never source jlo's help output ([907686b](https://github.com/java-loader/jlo/commit/907686b9158e13a20e813b4b5100d62faf4730b8))

## [0.2.0](https://github.com/java-loader/jlo/compare/jlo-bin-v0.1.0...jlo-bin-v0.2.0) (2026-03-11)


### Features

* add conditional environment setup for fresh shells ([22de6aa](https://github.com/java-loader/jlo/commit/22de6aa8de5c3bd50a9a9fa85e9dae050884b76e))
* add default command ([55d6ccb](https://github.com/java-loader/jlo/commit/55d6ccbe7bcab8b3c824a291275dc7ece2e030f8))


### Bug Fixes

* eliminate remaining unwraps and panics in error handling ([3985a7c](https://github.com/java-loader/jlo/commit/3985a7cc22626724f18d576fd5f85a0d5388988c))
* improve installation message clarity ([81f2594](https://github.com/java-loader/jlo/commit/81f2594bd609e8feac2be315578f9144bc37ccfa))
* prevent duplicate autoload triggers in bash ([7a3bb7d](https://github.com/java-loader/jlo/commit/7a3bb7da439f3c416c3640b7638cd8fef0b5b789))
* remove duplicate call ([4fb3e6a](https://github.com/java-loader/jlo/commit/4fb3e6a9698d3d16dd9f43c9926824b7dc01e7b3))
