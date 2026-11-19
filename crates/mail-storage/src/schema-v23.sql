-- 新默认：发送后保存到发件箱。升级时一次性启用，之后用户仍可关闭。
-- 仅更新未来任务的偏好，不改变已确认任务的冻结策略，也不补发历史邮件。
UPDATE desktop_settings SET save_sent_copy=1 WHERE id=1 AND save_sent_copy=0;
