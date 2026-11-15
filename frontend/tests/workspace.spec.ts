import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';

const fixture = JSON.parse(readFileSync('../test-results/e2e.json', 'utf8'));
test.beforeEach(async ({ page }) => {
  await page.goto(`/#token=${fixture.token}`);
  await expect(page.locator('.sidebar-footer')).toContainText('本地服务在线');
});

test('project creation, validation, editing and persistent queue controls', async ({ page }) => {
  await page.getByRole('button', { name: '＋ 新建项目', exact: true }).click();
  const dialog = page.getByRole('dialog');
  await dialog.getByLabel('项目名称').fill('界面管理验证');
  await dialog.getByLabel('本机代码目录').fill(fixture.workspace);
  await dialog.getByLabel('飞书 chat_id').fill('invalid');
  await dialog.getByRole('button', { name: '创建项目 →' }).click();
  await expect(page.getByRole('alert')).toContainText('oc_');
  await dialog.getByLabel('飞书 chat_id').fill('');
  await dialog.getByRole('button', { name: '创建项目 →' }).click();
  await expect(dialog).not.toBeVisible();
  await expect(page.locator('.detail-heading h2')).toHaveText('界面管理验证');
  await page.getByRole('button', { name: '编辑项目', exact: true }).click();
  await page.getByRole('dialog').getByLabel('项目名称').fill('界面管理验证 · 已编辑');
  await page.getByRole('button', { name: '保存修改', exact: true }).click();
  await expect(page.locator('.detail-heading h2')).toHaveText('界面管理验证 · 已编辑');
  await page.getByLabel('向主会话发送本地消息').fill('未启动的项目应持久排队');
  await page.getByRole('button', { name: '加入队列 ↑' }).click();
  await expect(page.locator('.message').filter({ hasText: '未启动的项目应持久排队' })).toContainText('等待投递');
  page.once('dialog', d => d.accept());
  await page.locator('.message').filter({ hasText: '未启动的项目应持久排队' }).getByRole('button', { name: '忽略', exact: true }).click();
  await expect(page.locator('.message').filter({ hasText: '未启动的项目应持久排队' })).toContainText('已忽略');
  await page.reload();
  await expect(page.locator('.sidebar-footer')).toContainText('本地服务在线');
  await expect(page.locator('.project-card').filter({ hasText: '界面管理验证 · 已编辑' })).toBeVisible();
});

test('manual token entry remains editable until submitted',async({page})=>{
  await page.evaluate(()=>sessionStorage.clear());
  await page.goto('/');
  const field=page.getByLabel('本机管理令牌');
  await field.fill(fixture.token);
  await expect(field).toBeVisible();
  await page.getByRole('button',{name:'连接工作台 →'}).click();
  await expect(page.locator('.sidebar-footer')).toContainText('本地服务在线');
});

test('desktop workspace has real message activity and no browser errors', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', e => errors.push(e.message));
  await page.locator('.project-card').filter({ hasText: '端到端验证项目' }).click();
  await expect(page.locator('.message').first()).toBeVisible();
  await expect(page.locator('.project-detail')).toContainText('已接收');
  await page.screenshot({ path: '../test-results/desktop.png', fullPage: true });
  expect(errors).toEqual([]);
});

test('mobile workspace and masked connection settings', async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.locator('.project-card').filter({ hasText: '端到端验证项目' }).click();
  await expect(page.locator('.detail-heading h2')).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(390);
  await page.screenshot({ path: '../test-results/mobile.png', fullPage: true });
  await page.getByRole('button', { name: '连接设置' }).click();
  await expect(page.getByLabel('App ID', { exact: true })).toHaveValue('cli_fixture');
  await expect(page.getByLabel('App Secret', { exact: true })).toHaveValue('');
  expect(await page.content()).not.toContain('fixture-secret');
  await page.screenshot({ path: '../test-results/settings.png', fullPage: true });
});
