RemoteConnectionSystem, пакет релиза, сборка RCS_BUILD_ID

Состав: bin\ — rcs-web, rcs-agent, rcs-supervisor с PDB; web\ — веб-клиент; deploy\ — установка и Caddy;
docs\RECOVERY.md — обновление, откат и восстановление; SHA256SUMS.txt — контрольные суммы. Секретов в пакете нет.

Обновление, от имени администратора в папке пакета:
   powershell -ExecutionPolicy Bypass -File .\deploy\install.ps1

Откат на версию, стоявшую до обновления:
   powershell -ExecutionPolicy Bypass -File .\deploy\install.ps1 -Rollback

Пароль, TOTP, журналы и файлы в D:\RCS-Storage обновление не трогает. Подробности и действия при сбоях —
в docs\RECOVERY.md.
